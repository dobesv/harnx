//! Bounded terminal persistence recovery after a turn fails.
use super::{Publisher, FAILED_MESSAGE};
use a2a_lf::{StreamResponse, TaskArtifactUpdateEvent, TaskState};
use anyhow::{Context, Result};
use harnx_runtime::NatsSession;
use std::{sync::Arc, time::Duration};
use tracing::warn;

const TERMINAL_PERSIST_ATTEMPTS: usize = 4;
const TERMINAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const TERMINAL_PERSIST_TIMEOUT: Duration = Duration::from_secs(5);

impl Publisher {
    pub(super) async fn fail_turn(&mut self, session: &NatsSession, task_id: &str) {
        if let Err(error) = session.cancel_pending_turn().await {
            warn!(%task_id, %error, "failed turn cancellation failed");
        }
        if self.record.is_none() {
            return;
        }
        self.flush_failed_output(task_id).await;
        for attempt in 0..TERMINAL_PERSIST_ATTEMPTS {
            if self.persist_failure_attempt(task_id, attempt).await {
                return;
            }
            retry_delay(attempt).await;
        }
        // done is still signaled by DetachedTurn. Blocking waiters must verify
        // KV/reconcile or return an error, never wait for another write here.
        warn!(%task_id, "A2A terminal persistence retries exhausted");
    }
    async fn persist_failure(&mut self) -> Result<()> {
        // A lost PubAck can hide a successful write. Re-read before retrying CAS.
        let record = self
            .store
            .get_task(&self.storage_key, &self.record().task.id)
            .await?
            .context("task disappeared before terminal persistence")?;
        let terminal = record.task.status.state.is_terminal();
        self.record = Some(Arc::new(record));
        if !terminal {
            return self
                .set_status(TaskState::Failed, Some(FAILED_MESSAGE))
                .await;
        }
        // Re-send an authoritative replacement if the previous final flush had
        // an ambiguous result. Replacement, unlike append, cannot duplicate text.
        if let Some(answer) = self
            .record()
            .task
            .artifacts
            .as_ref()
            .and_then(|artifacts| artifacts.first())
            .cloned()
        {
            self.send(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: self.record().task.id.clone(),
                context_id: self.record().task.context_id.clone(),
                artifact: answer,
                append: Some(false),
                last_chunk: Some(true),
                metadata: None,
            }));
        }
        self.send_status();
        Ok(())
    }
    async fn flush_failed_output(&mut self, task_id: &str) {
        if !self.output.sent && self.output.text.is_empty() {
            return;
        }
        if let Err(error) = self.flush_artifact(true, false).await {
            warn!(%task_id, %error, "A2A final artifact persistence failed");
        }
    }
    async fn persist_failure_attempt(&mut self, task_id: &str, attempt: usize) -> bool {
        match tokio::time::timeout(TERMINAL_PERSIST_TIMEOUT, self.persist_failure()).await {
            Ok(Ok(())) => return true,
            Ok(Err(error)) => {
                warn!(%task_id, %error, attempt = attempt + 1, "A2A terminal persistence failed")
            }
            Err(_) => warn!(%task_id, attempt = attempt + 1, "A2A terminal persistence timed out"),
        }
        false
    }
}

async fn retry_delay(attempt: usize) {
    if attempt + 1 < TERMINAL_PERSIST_ATTEMPTS {
        tokio::time::sleep(TERMINAL_RETRY_DELAY * (1 << attempt)).await;
    }
}
