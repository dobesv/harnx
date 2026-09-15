//! Keep one remote run bound to its original prompt through the output queue.
use anyhow::{Context, Result};
use harnx_execution_control::{ExecutionStore, OperationRef};
use harnx_runtime::nats_event_sink::{JetstreamContext, LiveEventState, SessionEventStream};
use tokio_stream::StreamExt;

pub(super) struct RemoteGeneration {
    store: ExecutionStore,
    reference: OperationRef,
    live: LiveEventState,
}

impl RemoteGeneration {
    pub(super) async fn bind(
        stream: &mut SessionEventStream,
        jetstream: &JetstreamContext,
        session: &str,
    ) -> Result<Option<Self>> {
        let generation = match stream.history_generation().await {
            Ok(generation) => generation,
            Err(error) => {
                log::debug!("remote prompt generation unavailable; history only: {error:#}");
                None
            }
        };
        let Some(generation) = generation else {
            // Legacy history may still be read. It cannot acquire live authority
            // later merely because another client starts a registered generation.
            stream.live_state().retire();
            return Ok(None);
        };
        stream.follow_generation(generation.clone());
        let store = ExecutionStore::from_store(
            jetstream
                .get_key_value(harnx_execution_control::BUCKET)
                .await?,
        );
        Ok(Some(Self {
            store,
            reference: OperationRef::new(session, generation),
            live: stream.live_state().clone(),
        }))
    }

    async fn wait_for_stop(&self) -> Result<()> {
        let mut updates = self.store.watch().await?;
        loop {
            if self.store.accepted_stop(&self.reference).await?.is_some() {
                self.live.stop(&self.reference.execution_id);
                return Ok(());
            }
            updates
                .next()
                .await
                .context("remote root stop watch closed")??;
        }
    }
}

pub(super) async fn wait_for_stop(generation: Option<RemoteGeneration>) -> Result<()> {
    match generation {
        Some(generation) => generation.wait_for_stop().await,
        None => std::future::pending().await,
    }
}
