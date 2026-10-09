//! Independent leader-backed task cursors. No queue or server consumer is shared.
use super::*;
use crate::store::context::{terminal_event, PendingEvent};
use async_nats::jetstream::stream::{RawMessageErrorKind, Stream};

const READ_INTERVAL: Duration = Duration::from_millis(100);

impl Runner {
    pub(super) async fn shared_snapshot(
        &self,
        export: &Export,
        owner: &RequestIdentity,
        task_id: &str,
    ) -> Result<(TaskRecord, Option<broadcast::Receiver<A2aEvent>>)> {
        let (local, _) = crate::store::parse_task_id(task_id)?;
        let storage = self
            .store
            .resolve_context(export, owner, local)
            .await?
            .ok_or(StoreError::NotFound)?;
        let mut stream = terminal_event::ensure_stream(&self.store).await?;
        // This order is deliberate. A newer watermark after snapshot would skip
        // an update (including terminal) committed in the handoff gap.
        let global = stream.info().await?.state.last_sequence;
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::WatermarkCaptured)
            .await;
        let snapshot = self
            .store
            .get_task(&storage, task_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::SnapshotCaptured)
            .await;
        if snapshot.task.status.state.is_terminal() {
            return Ok((snapshot, None));
        }
        let subject = terminal_event::task_subject(&storage, task_id)?;
        let (sender, receiver) = broadcast::channel(EVENT_CAPACITY);
        let reader = TaskReader {
            store: self.store.clone(),
            stream,
            storage,
            task: task_id.into(),
            subject,
            cursor: global + 1,
            sequence: snapshot.stream_seq,
            sender,
        };
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::ReaderCreated)
            .await;
        tokio::spawn(async move {
            let delivery = reader.sender.clone();
            let result = tokio::select! {
                result = reader.run() => result,
                _ = delivery.closed() => Ok(()),
            };
            if let Err(error) = result {
                warn!(%error, "A2A task reader interrupted; reconnect");
            }
        });
        Ok((snapshot, Some(receiver)))
    }
}

struct TaskReader {
    store: Arc<A2aStore>,
    stream: Stream,
    storage: String,
    task: String,
    subject: String,
    cursor: u64,
    sequence: u64,
    sender: broadcast::Sender<A2aEvent>,
}
impl TaskReader {
    async fn run(mut self) -> Result<()> {
        loop {
            match self
                .stream
                .raw_message_builder()
                .sequence(self.cursor)
                .next_by_subject(&self.subject)
                .send()
                .await
            {
                Ok(raw) => {
                    self.cursor = raw.sequence + 1;
                    let event: PendingEvent = serde_json::from_slice(&raw.payload)?;
                    if event.task_sequence <= self.sequence {
                        continue;
                    }
                    anyhow::ensure!(
                        event.task_sequence == self.sequence + 1,
                        "task event retention gap"
                    );
                    let task = match &event.response {
                        StreamResponse::ArtifactUpdate(update) => &update.task_id,
                        StreamResponse::StatusUpdate(update) => &update.task_id,
                        StreamResponse::Task(task) => &task.id,
                        _ => anyhow::bail!("unexpected task event payload"),
                    };
                    anyhow::ensure!(task == &self.task, "task event subject identity mismatch");
                    self.sequence = event.task_sequence;
                    let event = A2aEvent {
                        sequence: event.task_sequence,
                        response: event.response,
                    };
                    let terminal = event.is_terminal();
                    if self.sender.send(event).is_err() || terminal {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == RawMessageErrorKind::NoMessageFound => {
                    self.check_gap().await?;
                    tokio::time::sleep(READ_INTERVAL).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    async fn check_gap(&self) -> Result<()> {
        let context = self.store.read_context(&self.storage).await?;
        let confirmed = if let Some(active) = context
            .as_ref()
            .and_then(|c| c.document.state.active.as_ref())
            .filter(|active| active.snapshot.task.id == self.task)
        {
            active
                .publication
                .pending
                .as_ref()
                .map_or(active.publication.stream_seq, |pending| {
                    pending.task_sequence - 1
                })
        } else {
            let record = self
                .store
                .get_task(&self.storage, &self.task)
                .await?
                .context("task disappeared during streaming")?;
            anyhow::ensure!(
                record.task.status.state.is_terminal(),
                "task authority disappeared during streaming"
            );
            record.stream_seq
        };
        if confirmed > self.sequence {
            // Publication may have landed between the no-message read and its
            // checkpoint read. Re-read after confirmed progress before calling gap.
            self.stream
                .raw_message_builder()
                .sequence(self.cursor)
                .next_by_subject(&self.subject)
                .send()
                .await
                .context("task event retention gap (including terminal)")?;
        } else if let Some(active) = context
            .as_ref()
            .and_then(|context| context.document.state.active.as_ref())
            .filter(|active| active.snapshot.task.id == self.task)
        {
            if active.publication.subject_sequence > 0 {
                // Snapshot may cover the lost event, but loss of the latest
                // subject predecessor prevents all future conditional publication.
                let tail = self
                    .stream
                    .get_last_raw_message_by_subject(&self.subject)
                    .await
                    .context("task event subject predecessor missing; reconnect")?;
                anyhow::ensure!(
                    tail.sequence >= active.publication.subject_sequence,
                    "task event stream reset or predecessor retention gap"
                );
            }
        }
        Ok(())
    }
}
