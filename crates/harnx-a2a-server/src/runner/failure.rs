//! Bounded terminal persistence recovery after a turn fails.
use super::{Publisher, FAILED_MESSAGE};
use a2a_lf::TaskState;
use anyhow::{Context, Result};
use harnx_runtime::NatsSession;
use std::{sync::Arc, time::Duration};
use tracing::warn;

const TERMINAL_PERSIST_ATTEMPTS: usize = 4;
const TERMINAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const TERMINAL_PERSIST_TIMEOUT: Duration = Duration::from_secs(5);

impl Publisher {
    pub(super) async fn fail_turn(&mut self, session: &NatsSession, task_id: &str) {
        if let Err(error) = self.stop_failed_admission(session).await {
            warn!(%task_id, %error, "failed turn stop unconfirmed; retaining admission");
            return;
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
        let context = self
            .store
            .read_context(&self.storage_key)
            .await?
            .context("failure authority missing")?;
        anyhow::ensure!(
            context.document.owner == self.authority.context.document.owner
                && context
                    .document
                    .state
                    .active
                    .as_ref()
                    .map(|a| &a.snapshot.task.id)
                    == Some(&self.record().task.id),
            "failed task authority superseded"
        );
        let record = context
            .document
            .state
            .active
            .as_ref()
            .context("failure task missing")?
            .snapshot
            .clone();
        self.authority.context = context;
        let terminal = record.task.status.state.is_terminal();
        self.record = Some(Arc::new(record));
        if !terminal {
            return self.persist_failure_status().await;
        }
        self.authority
            .project_terminal(&self.store, &self.session)
            .await?;
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
    async fn persist_failure_status(&mut self) -> Result<()> {
        let ticket = self.authority.ticket(&self.storage_key)?;
        let completed = self.session.fixed_prompt_completion(&ticket).await?;
        let canceled = self
            .authority
            .context
            .document
            .state
            .active
            .as_ref()
            .is_some_and(|active| active.cancel.is_some());
        let (state, text) = match completed {
            Some(result) if !result.was_cancelled => super::turn_outcome(result, &self.output.text),
            _ if canceled => (TaskState::Canceled, None),
            _ => (TaskState::Failed, Some(FAILED_MESSAGE.into())),
        };
        let (state, text) = super::limits::bounded_outcome(
            (state, text),
            super::limits::task_output_limit(&self.store, &self.authority.context).await?,
        );
        if state == TaskState::Completed {
            self.output.text = text.clone().unwrap_or_default();
        }
        self.set_status(state, text.as_deref()).await
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
