//! Purge only checkpoint-covered history, preserving each retained subject tail.
use super::{context::PendingEvent, A2aStore};
use a2a_lf::StreamResponse;
use anyhow::{ensure, Result};
use async_nats::jetstream::stream::RawMessageErrorKind;

impl A2aStore {
    pub async fn cleanup_context_events(&self, storage: &str) -> Result<u64> {
        let Some(context) = self.read_context(storage).await? else {
            return Ok(0);
        };
        let Some(active) = &context.document.state.active else {
            return Ok(0);
        };
        let history = &active.publication.confirmed_history;
        let Some(&floor) = history
            .first()
            .filter(|_| history.len() == harnx_runtime::a2a_events::CHECKPOINT_HISTORY)
        else {
            return Ok(0);
        };
        ensure!(
            floor > 0 && floor <= active.publication.subject_sequence,
            "unconfirmed event purge cutoff"
        );
        // NATS 2.11.6 treats a filtered purge with seq=1 as a full purge.
        if floor <= 1 {
            return Ok(0);
        }
        let stream = super::context::terminal_event::ensure_stream(self).await?;
        let subject =
            super::context::terminal_event::task_subject(storage, &active.snapshot.task.id)?;
        // Purge excludes floor. Concurrent publication and old purge retries stay above it.
        let purged = stream.purge().filter(subject).sequence(floor).await?.purged;
        metrics::counter!("harnx_a2a_events_purged_total", "phase" => "checkpoint")
            .increment(purged);
        Ok(purged)
    }

    pub(crate) async fn cleanup_terminal_events(&self, cursor: &mut u64) -> Result<()> {
        self.cleanup_terminal_events_at(cursor, chrono::Utc::now().timestamp())
            .await
    }

    #[cfg(feature = "fault-injection")]
    pub async fn cleanup_terminal_events_for_test(&self, cursor: &mut u64, now: i64) -> Result<()> {
        self.cleanup_terminal_events_at(cursor, now).await
    }

    async fn cleanup_terminal_events_at(&self, cursor: &mut u64, now: i64) -> Result<()> {
        let stream = super::context::terminal_event::ensure_stream(self).await?;
        for _ in 0..32 {
            let raw = match stream
                .raw_message_builder()
                .sequence(*cursor)
                .next_by_subject("a2a.tasks.>")
                .send()
                .await
            {
                Ok(raw) => raw,
                Err(error) if error.kind() == RawMessageErrorKind::NoMessageFound => {
                    *cursor = 0;
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            *cursor = raw.sequence + 1;
            self.cleanup_event(&stream, &raw, now).await?;
        }
        Ok(())
    }
}

impl A2aStore {
    async fn cleanup_event(
        &self,
        stream: &async_nats::jetstream::stream::Stream,
        raw: &async_nats::jetstream::message::StreamMessage,
        now: i64,
    ) -> Result<()> {
        if now - raw.time.unix_timestamp() < 30 {
            return Ok(());
        }
        let event: PendingEvent = serde_json::from_slice(&raw.payload)?;
        if self.cleanup_deleted_event(stream, raw, &event).await? {
            return Ok(());
        }
        let StreamResponse::StatusUpdate(status) = &event.response else {
            return Ok(());
        };
        if !status.status.state.is_terminal() {
            return Ok(());
        }
        let Some(storage) = event_storage(raw) else {
            return Ok(());
        };
        ensure!(
            super::context::terminal_event::task_subject(storage, &status.task_id)?
                == raw.subject.as_str(),
            "event cleanup subject mismatch"
        );
        if raw.sequence > 1
            && self
                .terminal_checkpoint(storage, &status.task_id, event.task_sequence)
                .await?
        {
            let purged = stream
                .purge()
                .filter(raw.subject.as_str())
                .sequence(raw.sequence)
                .await?
                .purged;
            metrics::counter!("harnx_a2a_events_purged_total", "phase" => "terminal")
                .increment(purged);
        }
        Ok(())
    }
}

impl A2aStore {
    async fn terminal_checkpoint(&self, storage: &str, task: &str, sequence: u64) -> Result<bool> {
        let context = self.read_context(storage).await?;
        if context
            .as_ref()
            .and_then(|doc| doc.document.state.active.as_ref())
            .is_some_and(|active| active.snapshot.task.id == task && !active.ready_to_retire())
        {
            return Ok(false);
        }
        Ok(self.get_task(storage, task).await?.is_some_and(|task| {
            task.task.status.state.is_terminal() && task.stream_seq == sequence
        }))
    }

    async fn cleanup_deleted_event(
        &self,
        stream: &async_nats::jetstream::stream::Stream,
        raw: &async_nats::jetstream::message::StreamMessage,
        event: &PendingEvent,
    ) -> Result<bool> {
        // A delayed first committed publish has predecessor zero and can land
        // after GC removed its whole subject and the broker dedupe window elapsed.
        if event.task_sequence != 1 || event.expected_subject_sequence != 0 {
            return Ok(false);
        }
        let Some(storage) = event_storage(raw) else {
            return Ok(false);
        };
        let key = super::context::context_authority_key(storage);
        let entry =
            harnx_nats_common::leader_reads::entry(self.metadata().kv_store(), &key).await?;
        if entry.is_some_and(|entry| entry.operation != async_nats::jetstream::kv::Operation::Put) {
            let purged = stream
                .purge()
                .filter(raw.subject.as_str())
                .sequence(raw.sequence + 1)
                .await?
                .purged;
            metrics::counter!("harnx_a2a_events_purged_total", "phase" => "deleted")
                .increment(purged);
            return Ok(true);
        }
        Ok(false)
    }
}

fn event_storage(raw: &async_nats::jetstream::message::StreamMessage) -> Option<&str> {
    raw.subject
        .as_str()
        .rsplit_once('.')
        .and_then(|(prefix, _)| prefix.strip_prefix("a2a.tasks."))
}
