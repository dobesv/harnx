//! A durable approval boundary parks work; publication failure isn't execution failure.
use super::*;
use crate::store::context::{HitlCheckpoint, HitlPhase};
use harnx_runtime::{PendingHitlApproval, PromptProgress};

impl Publisher {
    pub(super) async fn handle_execution_error(
        &mut self,
        session: &NatsSession,
        task_id: &str,
        error: &anyhow::Error,
    ) -> bool {
        if !self.suspending {
            // A coalescer publish can fail at the same instant the worker parks,
            // before select! observes the typed boundary. Durable HITL wins.
            if let Ok(ticket) = self.authority.ticket(&self.storage_key) {
                if let Ok(PromptProgress::InputRequired(pending)) =
                    session.fixed_prompt_progress(&ticket).await
                {
                    self.suspending = true;
                    if self.suspend(pending).await.is_ok() {
                        return true;
                    }
                }
            }
        }
        if self.suspending {
            warn!(%task_id, %error, "A2A suspension publication unresolved; retained for retry");
            // Don't release ownership before the outbox drains. Dropping the
            // lease stops renewal; a successor retries the retained envelope.
            false
        } else {
            warn!(%task_id, %error, "A2A turn failed");
            self.fail_turn(session, task_id).await;
            true
        }
    }

    pub(super) async fn drain_outbox(&mut self) -> Result<()> {
        if let Some(event) = self
            .authority
            .flush_pending(&self.store, &self.storage_key)
            .await?
        {
            let _ = self.events.send(event);
        }
        self.record = self
            .authority
            .context
            .document
            .state
            .active
            .as_ref()
            .map(|active| Arc::new(active.snapshot.clone()));
        self.publish_snapshot();
        Ok(())
    }

    pub(super) async fn suspend(&mut self, pending: PendingHitlApproval) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        self.hooks
            .checkpoint(crate::fault_injection::Boundary::HitlRequestObserved)
            .await;
        // Retries reuse the committed envelope and replacement artifact, never
        // append the same text twice or invent another challenge generation.
        for attempt in 0..3 {
            match self.persist_suspension(&pending).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    warn!(%error, attempt, "A2A suspension persistence will retry");
                    if attempt == 2 || !self.authority.lease.revalidate_ownership().await? {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        unreachable!()
    }

    async fn persist_suspension(&mut self, pending: &PendingHitlApproval) -> Result<()> {
        self.drain_outbox().await?;
        self.reconcile_output(Some(pending.request_seq)).await?;
        let saved_text = self
            .record()
            .task
            .artifacts
            .as_ref()
            .and_then(|artifacts| artifacts.first())
            .map(|artifact| super::durable_output::part_text(&artifact.parts));
        if saved_text.as_deref() == Some(&self.output.text) {
            self.output.committed();
        } else {
            self.flush_artifact(false, true).await?;
        }
        self.drain_outbox().await?;
        let request =
            pending.approval_ref(self.authority.ticket(&self.storage_key)?.invocation_id());
        let challenge = status(
            TaskState::InputRequired,
            Some(&crate::hitl::render_challenge(pending)),
        );
        let commit_id = format!(
            "{}:hitl:{}:input-required",
            self.record().task.id,
            pending.request_seq
        );
        let record = self
            .authority
            .update(&self.store, &self.storage_key, |active| {
                // A stage/clear acknowledgement can be lost after the CAS. Its
                // generation proves the suspension was already committed.
                if active
                    .hitl_checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.request == request)
                {
                    return;
                }
                active.snapshot.task.status = challenge.clone();
                active.hitl_checkpoint = Some(HitlCheckpoint {
                    request: request.clone(),
                    phase: HitlPhase::Waiting,
                    reply: None,
                    observed_log_seq: pending.request_seq,
                });
                super::outbox::stage_event(
                    active,
                    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                        task_id: active.snapshot.task.id.clone(),
                        context_id: active.snapshot.task.context_id.clone(),
                        status: active.snapshot.task.status.clone(),
                        metadata: None,
                    }),
                    &commit_id,
                );
            })
            .await?;
        self.record = Some(Arc::new(record));
        self.publish_snapshot();
        self.drain_outbox().await
    }
}
