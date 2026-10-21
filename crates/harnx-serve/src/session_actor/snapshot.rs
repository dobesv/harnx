use super::*;
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
use std::future::Future;

struct TurnEventSender {
    tx: mpsc::UnboundedSender<TurnEvent>,
    config: GlobalConfig,
}

struct TurnEvent {
    event: AgentEvent,
    through_seq: Option<u64>,
}

impl AgentEventSink for TurnEventSender {
    fn emit(&self, event: AgentEvent) {
        // Capture before the runtime resumes: reading the log later can include
        // a response whose streaming events are still behind this snapshot.
        // Synchronous capture fences the snapshot to the compaction's `through_seq`,
        // preventing mid-turn history snapshots from seeing future entries.
        let through_seq = matches!(
            event,
            AgentEvent::Session(SessionEvent::CompactingCompleted { .. })
        )
        .then(|| {
            self.config
                .read()
                .session
                .as_ref()
                .map(|s| s.next_seq() as u64)
        })
        .flatten();
        let _ = self.tx.send(TurnEvent { event, through_seq });
    }
}

/// Refresh compaction from the durable log without blocking a synchronous sink
/// callback. Drain before returning so the actor's terminal event can't overtake it.
pub(super) struct TurnEventRelay {
    rx: mpsc::UnboundedReceiver<TurnEvent>,
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
        (
            Arc::new(TurnEventSender {
                tx,
                config: params.prompt_config.clone(),
            }),
            relay,
        )
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

    async fn forward(&self, TurnEvent { event, through_seq }: TurnEvent) {
        let AgentEvent::Session(completed @ SessionEvent::CompactingCompleted { .. }) = event
        else {
            self.sink.emit(event);
            return;
        };
        let snapshot = self
            .load_snapshot(through_seq)
            .await
            .map_err(|error| log::warn!("Failed to refresh compacted transcript: {error:#}"))
            .ok();
        self.sink
            .sink
            .emit_compacting_completed(completed, snapshot);
    }

    async fn load_snapshot(
        &self,
        through_seq: Option<u64>,
    ) -> anyhow::Result<crate::transcript_snapshot::HistorySnapshot> {
        let config = self.config.read().clone();
        let loaded =
            crate::load_nats_session_state(&config, self.key.target(), self.key.session()).await?;
        let end = through_seq.map_or(loaded.entries.len(), |seq| {
            loaded
                .entries
                .partition_point(|(entry_seq, _)| *entry_seq <= seq)
        });
        crate::transcript_snapshot::HistorySnapshot::from_entries(
            &loaded.entries[..end],
            &loaded.base_session,
            loaded.worker_active,
        )
    }
}

#[cfg(test)]
mod tests;
