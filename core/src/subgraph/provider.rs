use futures01::sync::mpsc::{channel, Receiver, Sender};
use std::collections::HashSet;
use std::sync::Mutex;
use std::ops::Deref as _;

use graph::data::subgraph::schema::attribute_index_definitions;
use graph::prelude::{
    DataSourceLoader as _, GraphQlRunner,
    SubgraphAssignmentProvider as SubgraphAssignmentProviderTrait, *,
    DynTryFut,
};

use crate::subgraph::registrar::IPFS_SUBGRAPH_LOADING_TIMEOUT;
use crate::DataSourceLoader;

pub struct SubgraphAssignmentProvider<L, Q, S> {
    logger_factory: LoggerFactory,
    event_stream: Option<Receiver<SubgraphAssignmentProviderEvent>>,
    event_sink: Sender<SubgraphAssignmentProviderEvent>,
    resolver: Arc<L>,
    subgraphs_running: Arc<Mutex<HashSet<SubgraphDeploymentId>>>,
    store: Arc<S>,
    graphql_runner: Arc<Q>,
}

impl<L, Q, S> SubgraphAssignmentProvider<L, Q, S>
where
    L: LinkResolver + Clone,
    Q: GraphQlRunner,
    S: Store,
{
    pub fn new(
        logger_factory: &LoggerFactory,
        resolver: Arc<L>,
        store: Arc<S>,
        graphql_runner: Arc<Q>,
    ) -> Self {
        let (event_sink, event_stream) = channel(100);

        let logger = logger_factory.component_logger("SubgraphAssignmentProvider", None);
        let logger_factory = logger_factory.with_parent(logger.clone());

        // Create the subgraph provider
        SubgraphAssignmentProvider {
            logger_factory,
            event_stream: Some(event_stream),
            event_sink,
            resolver: Arc::new(
                resolver
                    .as_ref()
                    .clone()
                    .with_timeout(*IPFS_SUBGRAPH_LOADING_TIMEOUT)
                    .with_retries(),
            ),
            subgraphs_running: Arc::new(Mutex::new(HashSet::new())),
            store,
            graphql_runner,
        }
    }

    /// Clones but forcing receivers to `None`.
    fn clone_no_receivers(&self) -> Self {
        SubgraphAssignmentProvider {
            event_stream: None,
            event_sink: self.event_sink.clone(),
            resolver: self.resolver.clone(),
            subgraphs_running: self.subgraphs_running.clone(),
            store: self.store.clone(),
            graphql_runner: self.graphql_runner.clone(),
            logger_factory: self.logger_factory.clone(),
        }
    }
}

impl<L, Q, S> SubgraphAssignmentProviderTrait for SubgraphAssignmentProvider<L, Q, S>
where
    L: LinkResolver + Clone,
    Q: GraphQlRunner,
    S: Store + SubgraphDeploymentStore,
{
    fn start<'a>(
        &'a self,
        id: &'a SubgraphDeploymentId,
    ) -> DynTryFut<'a, (), SubgraphAssignmentProviderError> {
        let self_clone = self.clone_no_receivers();
        let store = self.store.clone();
        let subgraph_id = id.clone();

        let loader = Arc::new(DataSourceLoader::new(
            store.clone(),
            self.resolver.clone(),
            self.graphql_runner.clone(),
        ));

        let link = format!("/ipfs/{}", id);

        let logger = self.logger_factory.subgraph_logger(id);
        let logger_for_resolve = logger.clone();
        let logger_for_err = logger.clone();
        let resolver = self.resolver.clone();

        info!(logger, "Resolve subgraph files using IPFS");

        Box::pin(async move {
            let mut subgraph = SubgraphManifest::resolve(Link { link }, resolver.deref(), &logger_for_resolve)
                .map_err(SubgraphAssignmentProviderError::ResolveError).await?;

            let data_sources = loader
                .load_dynamic_data_sources(id, logger.clone())
                .compat()
                .map_err(SubgraphAssignmentProviderError::DynamicDataSourcesError).await?;

            info!(logger, "Successfully resolved subgraph files using IPFS");

            // Add dynamic data sources to the subgraph
            subgraph.data_sources.extend(data_sources);

            // If subgraph ID already in set
            if !self_clone
                .subgraphs_running
                .lock()
                .unwrap()
                .insert(subgraph.id.clone())
            {
                info!(logger, "Subgraph deployment is already running");

                return Err(SubgraphAssignmentProviderError::AlreadyRunning(subgraph.id));
            }

            info!(logger, "Create attribute indexes for subgraph entities");

            // Build indexes for each entity attribute in the Subgraph
            let index_definitions = attribute_index_definitions(
                subgraph.id.clone(),
                subgraph.schema.document.clone(),
            );
            self_clone.store
                .build_entity_attribute_indexes(&subgraph.id, index_definitions)
                .map(|_| {
                    info!(
                        logger,
                        "Successfully created attribute indexes for subgraph entities"
                    )
                })
                .ok();

            // Send events to trigger subgraph processing
            if let Err(e) = self_clone
                .event_sink
                .clone()
                .send(SubgraphAssignmentProviderEvent::SubgraphStart(subgraph))
                .compat()
                .await {
                    panic!("failed to forward subgraph: {}", e);
                }
            Ok(())
        }.map_err(move |e| {
            error!(
                logger_for_err,
                "Failed to resolve subgraph files using IPFS";
                "error" => format!("{}", e)
            );

            let _ignore_error = store.apply_metadata_operations(
                SubgraphDeploymentEntity::update_failed_operations(&subgraph_id, true),
            );
            e
        }))
    }

    fn stop(
        &self,
        id: SubgraphDeploymentId,
    ) -> Box<dyn Future<Item = (), Error = SubgraphAssignmentProviderError> + Send + 'static> {
        // If subgraph ID was in set
        if self.subgraphs_running.lock().unwrap().remove(&id) {
            // Shut down subgraph processing
            Box::new(
                self.event_sink
                    .clone()
                    .send(SubgraphAssignmentProviderEvent::SubgraphStop(id))
                    .map_err(|e| panic!("failed to forward subgraph shut down event: {}", e))
                    .map(|_| ()),
            )
        } else {
            Box::new(future::err(SubgraphAssignmentProviderError::NotRunning(id)))
        }
    }
}

impl<L, Q, S> EventProducer<SubgraphAssignmentProviderEvent>
    for SubgraphAssignmentProvider<L, Q, S>
{
    fn take_event_stream(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = SubgraphAssignmentProviderEvent, Error = ()> + Send>> {
        self.event_stream.take().map(|s| {
            Box::new(s)
                as Box<dyn Stream<Item = SubgraphAssignmentProviderEvent, Error = ()> + Send>
        })
    }
}
