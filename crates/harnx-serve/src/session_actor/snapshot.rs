use super::*;
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
use std::future::Future;

struct TurnEventSender(mpsc::UnboundedSender<AgentEvent>);

impl AgentEventSink for TurnEventSender {
    fn emit(&self, event: AgentEvent) {
        let _ = self.0.send(event);
    }
}

/// Refresh compaction from the durable log without blocking a synchronous sink
/// callback. Drain before returning so the actor's terminal event can't overtake it.
pub(super) struct TurnEventRelay {
    rx: mpsc::UnboundedReceiver<AgentEvent>,
    sink: Arc<BroadcastEventSender>,
    config: GlobalConfig,
    key: SessionKey,
}

impl TurnEventRelay {
    pub(super) fn new(params: &ActorTurnParams) -> (Arc<dyn AgentEventSink>, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        let relay = Self {
            rx,
            sink: params.sink.clone(),
            config: params.prompt_config.clone(),
            key: params.key.clone(),
        };
        (Arc::new(TurnEventSender(tx)), relay)
    }

    pub(super) async fn run<T>(mut self, turn: impl Future<Output = T>) -> T {
        tokio::pin!(turn);
        let result = loop {
            tokio::select! {
                result = &mut turn => break result,
                Some(event) = self.rx.recv() => self.forward(event).await,
            }
        };
        while let Ok(event) = self.rx.try_recv() {
            self.forward(event).await;
        }
        result
    }

    async fn forward(&self, event: AgentEvent) {
        let AgentEvent::Session(completed @ SessionEvent::CompactingCompleted { .. }) = event
        else {
            self.sink.emit(event);
            return;
        };
        let snapshot = self
            .load_snapshot()
            .await
            .map_err(|error| log::warn!("Failed to refresh compacted transcript: {error:#}"))
            .ok();
        self.sink
            .sink
            .emit_compacting_completed(completed, snapshot);
    }

    async fn load_snapshot(&self) -> anyhow::Result<crate::transcript_snapshot::HistorySnapshot> {
        let config = self.config.read().clone();
        let loaded =
            crate::load_nats_session_state(&config, self.key.target(), self.key.session()).await?;
        crate::transcript_snapshot::HistorySnapshot::from_entries(
            &loaded.entries,
            &loaded.base_session,
            loaded.worker_active,
        )
    }
}

#[cfg(test)]
mod tests;
