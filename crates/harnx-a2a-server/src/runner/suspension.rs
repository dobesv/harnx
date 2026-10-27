//! A durable approval boundary parks work; publication failure isn't execution failure.
use super::*;
use crate::store::context::ActiveTask;
use crate::store::context::{HitlCheckpoint, HitlPhase};
use harnx_runtime::{PendingHitlApproval, PromptProgress};

impl Publisher {
    pub(super) async fn handle_execution_error(
        &mut self,
        session: &NatsSession,
        task_id: &str,
        error: &anyhow::Error,
    ) -> bool {
        if !self.suspending && self.try_durable_suspension(session).await {
            return true;
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

    async fn try_durable_suspension(&mut self, session: &NatsSession) -> bool {
        // A coalescer publish can fail before select! observes the worker's
        // approval boundary. Durable HITL still takes precedence over failure.
        let Ok(ticket) = self.authority.ticket(&self.storage_key) else {
            return false;
        };
        let Ok(PromptProgress::InputRequired(pending)) =
            session.fixed_prompt_progress(&ticket).await
        else {
            return false;
        };
        self.suspending = true;
        self.suspend(pending).await.is_ok()
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

    async fn sync_suspension_output(&mut self, request_seq: u64) -> Result<()> {
        self.drain_outbox().await?;
        self.reconcile_output(Some(request_seq)).await?;
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
        self.drain_outbox().await
    }

    async fn persist_suspension(&mut self, pending: &PendingHitlApproval) -> Result<()> {
        self.sync_suspension_output(pending.request_seq).await?;
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
        let draft = SuspensionDraft {
            request,
            challenge,
            pending_seq: pending.request_seq,
            commit_id: &commit_id,
        };
        let record = self
            .authority
            .update(&self.store, &self.storage_key, |active| {
                apply_suspension_mutation(active, &draft);
            })
            .await?;
        self.record = Some(Arc::new(record));
        self.publish_snapshot();
        self.drain_outbox().await
    }
}

struct SuspensionDraft<'a> {
    request: harnx_runtime::ApprovalRef,
    challenge: a2a_lf::TaskStatus,
    pending_seq: u64,
    commit_id: &'a str,
}

fn apply_suspension_mutation(active: &mut ActiveTask, draft: &SuspensionDraft<'_>) {
    if active
        .hitl_checkpoint
        .as_ref()
        .is_some_and(|checkpoint| checkpoint.request == draft.request)
    {
        return;
    }
    active.snapshot.task.status = draft.challenge.clone();
    active.hitl_checkpoint = Some(HitlCheckpoint {
        request: draft.request.clone(),
        phase: HitlPhase::Waiting,
        reply: None,
        observed_log_seq: draft.pending_seq,
    });
    super::outbox::stage_event(
        active,
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: active.snapshot.task.id.clone(),
            context_id: active.snapshot.task.context_id.clone(),
            status: active.snapshot.task.status.clone(),
            metadata: None,
        }),
        draft.commit_id,
    );
}
