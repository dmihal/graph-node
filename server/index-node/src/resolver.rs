use std::collections::{BTreeMap, BTreeSet, HashMap};

use graph::data::subgraph::features::validate_subgraph_features;
use graph::data::subgraph::status;
use graph::prelude::*;
use graph::{
    components::store::StatusStore,
    data::graphql::{IntoValue, ObjectOrInterface, ValueMap},
};
use graph_graphql::prelude::{ExecutionContext, Resolver};
use std::convert::TryInto;
use web3::types::{Address, H256};

/// Resolver for the index node GraphQL API.
pub struct IndexNodeResolver<S, R, St> {
    logger: Logger,
    store: Arc<S>,
    link_resolver: Arc<R>,
    subgraph_store: Arc<St>,
}

impl<S, R, St> IndexNodeResolver<S, R, St>
where
    S: StatusStore,
    R: LinkResolver,
    St: SubgraphStore,
{
    pub fn new(
        logger: &Logger,
        store: Arc<S>,
        link_resolver: Arc<R>,
        subgraph_store: Arc<St>,
    ) -> Self {
        let logger = logger.new(o!("component" => "IndexNodeResolver"));
        Self {
            logger,
            store,
            link_resolver,
            subgraph_store,
        }
    }

    fn resolve_indexing_statuses(
        &self,
        arguments: &HashMap<&str, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        let deployments = arguments
            .get("subgraphs")
            .map(|value| match value {
                q::Value::List(ids) => ids
                    .into_iter()
                    .map(|id| match id {
                        s::Value::String(s) => s.clone(),
                        _ => unreachable!(),
                    })
                    .collect(),
                _ => unreachable!(),
            })
            .unwrap_or_else(|| Vec::new());

        let infos = self
            .store
            .status(status::Filter::Deployments(deployments))?;
        Ok(infos.into_value())
    }

    fn resolve_indexing_statuses_for_subgraph_name(
        &self,
        arguments: &HashMap<&str, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        // Get the subgraph name from the arguments; we can safely use `expect` here
        // because the argument will already have been validated prior to the resolver
        // being called
        let subgraph_name = arguments
            .get_required::<String>("subgraphName")
            .expect("subgraphName not provided");

        debug!(
            self.logger,
            "Resolve indexing statuses for subgraph name";
            "name" => &subgraph_name
        );

        let infos = self
            .store
            .status(status::Filter::SubgraphName(subgraph_name))?;

        Ok(infos.into_value())
    }

    fn resolve_proof_of_indexing(
        &self,
        argument_values: &HashMap<&str, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        let deployment_id = argument_values
            .get_required::<DeploymentHash>("subgraph")
            .expect("Valid subgraphId required");

        let block_number: u64 = argument_values
            .get_required::<u64>("blockNumber")
            .expect("Valid blockNumber required")
            .try_into()
            .unwrap();

        let block_hash = argument_values
            .get_required::<H256>("blockHash")
            .expect("Valid blockHash required")
            .try_into()
            .unwrap();

        let block = BlockPtr::from((block_hash, block_number));

        let indexer = argument_values
            .get_optional::<Address>("indexer")
            .expect("Invalid indexer");

        let poi_fut =
            self.store
                .clone()
                .get_proof_of_indexing(&deployment_id, &indexer, block.clone());
        let poi = match futures::executor::block_on(poi_fut) {
            Ok(Some(poi)) => q::Value::String(format!("0x{}", hex::encode(&poi))),
            Ok(None) => q::Value::Null,
            Err(e) => {
                error!(
                    self.logger,
                    "Failed to query proof of indexing";
                    "subgraph" => deployment_id,
                    "block" => format!("{}", block),
                    "error" => format!("{:?}", e)
                );
                q::Value::Null
            }
        };

        Ok(poi)
    }

    fn resolve_indexing_status_for_version(
        &self,
        arguments: &HashMap<&str, q::Value>,

        // If `true` return the current version, if `false` return the pending version.
        current_version: bool,
    ) -> Result<q::Value, QueryExecutionError> {
        // We can safely unwrap because the argument is non-nullable and has been validated.
        let subgraph_name = arguments.get_required::<String>("subgraphName").unwrap();

        debug!(
            self.logger,
            "Resolve indexing status for subgraph name";
            "name" => &subgraph_name,
            "current_version" => current_version,
        );

        let infos = self.store.status(status::Filter::SubgraphVersion(
            subgraph_name,
            current_version,
        ))?;

        Ok(infos
            .into_iter()
            .next()
            .map(|info| info.into_value())
            .unwrap_or(q::Value::Null))
    }

    fn resolve_subgraph_features(
        &self,
        arguments: &HashMap<&str, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        // We can safely unwrap because the argument is non-nullable and has been validated.
        let subgraph_id = arguments.get_required::<String>("subgraphId").unwrap();

        todo!("try to fetch this subgraph from our SubgraphStore before hitting IPFS");

        // Try to build a deployment hash with the input string
        let deployment_hash = DeploymentHash::new(subgraph_id).map_err(|invalid_qm_hash| {
            QueryExecutionError::SubgraphDeploymentIdError(invalid_qm_hash)
        })?;

        // Try to fetch the subgraph manifest from IPFS. Since this operation is asynchronous, we
        // must wait for it to finish using the `block_on` function.
        let unvalidated_subgraph_manifest = {
            let future = UnvalidatedSubgraphManifest::<graph_chain_ethereum::Chain>::resolve(
                deployment_hash,
                self.link_resolver.clone(),
                &self.logger,
            );
            futures03::executor::block_on(future)
                .map_err(|_error| QueryExecutionError::SubgraphManifestResolveError)?
        };

        // We then need to validate the subgraph we've justo obtained
        let (subgraph_manifest, _) = unvalidated_subgraph_manifest
            .validate(self.subgraph_store.clone())
            .map_err(|_error| QueryExecutionError::InvalidSubgraphManifest)?;

        // We then bulid a GraphqQL `Object` value that contains the feature detection and
        // validation results and send it back as a response.
        let response = {
            // The response object will have either:
            // - a list of features for the "feature" key and a null value for the "error" key; OR
            // -. a an empty list for the "feature" key and a string for the "error" key.
            let mut detected_features = Vec::new();
            let mut error: q::Value = q::Value::Null;

            match validate_subgraph_features(&subgraph_manifest) {
                Ok(features) => features
                    .iter()
                    .map(ToString::to_string)
                    .map(q::Value::String)
                    .for_each(|feature| detected_features.push(feature)),
                Err(validation_error) => error = q::Value::String(validation_error.to_string()),
            }

            let mut response: BTreeMap<String, q::Value> = BTreeMap::new();
            response.insert("features".to_string(), q::Value::List(detected_features));
            response.insert("errors".to_string(), error);
            response
        };

        Ok(q::Value::Object(response))
    }
}

impl<S, R, St> Clone for IndexNodeResolver<S, R, St>
where
    S: SubgraphStore,
    R: LinkResolver,
    St: SubgraphStore,
{
    fn clone(&self) -> Self {
        Self {
            logger: self.logger.clone(),
            store: self.store.clone(),
            link_resolver: self.link_resolver.clone(),
            subgraph_store: self.subgraph_store.clone(),
        }
    }
}

#[async_trait]
impl<S, R, St> Resolver for IndexNodeResolver<S, R, St>
where
    S: StatusStore,
    R: LinkResolver,
    St: SubgraphStore,
{
    const CACHEABLE: bool = false;

    async fn query_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.store.query_permit().await
    }

    fn prefetch(
        &self,
        _: &ExecutionContext<Self>,
        _: &q::SelectionSet,
    ) -> Result<Option<q::Value>, Vec<QueryExecutionError>> {
        Ok(None)
    }

    /// Resolves a scalar value for a given scalar type.
    fn resolve_scalar_value(
        &self,
        parent_object_type: &s::ObjectType,
        field: &q::Field,
        scalar_type: &s::ScalarType,
        value: Option<q::Value>,
        argument_values: &HashMap<&str, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        // Check if we are resolving the proofOfIndexing bytes
        if &parent_object_type.name == "Query"
            && &field.name == "proofOfIndexing"
            && &scalar_type.name == "Bytes"
        {
            return self.resolve_proof_of_indexing(argument_values);
        }

        // Fallback to the same as is in the default trait implementation. There
        // is no way to call back into the default implementation for the trait.
        // So, note that this is duplicated.
        // See also c2112309-44fd-4a84-92a0-5a651e6ed548
        Ok(value.unwrap_or(q::Value::Null))
    }

    fn resolve_objects(
        &self,
        prefetched_objects: Option<q::Value>,
        field: &q::Field,
        _field_definition: &s::Field,
        object_type: ObjectOrInterface<'_>,
        arguments: &HashMap<&str, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        match (prefetched_objects, object_type.name(), field.name.as_str()) {
            // The top-level `indexingStatuses` field
            (None, "SubgraphIndexingStatus", "indexingStatuses") => {
                self.resolve_indexing_statuses(arguments)
            }

            // The top-level `indexingStatusesForSubgraphName` field
            (None, "SubgraphIndexingStatus", "indexingStatusesForSubgraphName") => {
                self.resolve_indexing_statuses_for_subgraph_name(arguments)
            }

            // Resolve fields of `Object` values (e.g. the `chains` field of `ChainIndexingStatus`)
            (value, _, _) => Ok(value.unwrap_or(q::Value::Null)),
        }
    }

    fn resolve_object(
        &self,
        prefetched_object: Option<q::Value>,
        field: &q::Field,
        _field_definition: &s::Field,
        _object_type: ObjectOrInterface<'_>,
        arguments: &HashMap<&str, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        match (prefetched_object, field.name.as_str()) {
            // The top-level `indexingStatusForCurrentVersion` field
            (None, "indexingStatusForCurrentVersion") => {
                self.resolve_indexing_status_for_version(arguments, true)
            }

            // The top-level `indexingStatusForPendingVersion` field
            (None, "indexingStatusForPendingVersion") => {
                self.resolve_indexing_status_for_version(arguments, false)
            }

            // The top-level `indexingStatusForPendingVersion` field
            (None, "subgraphFeatures") => self.resolve_subgraph_features(arguments),

            // Resolve fields of `Object` values (e.g. the `latestBlock` field of `EthereumBlock`)
            (value, _) => Ok(value.unwrap_or(q::Value::Null)),
        }
    }
}
