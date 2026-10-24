//! Continuations follow the original fixed ticket. Reply admission lives elsewhere.
use super::*;
use crate::store::context::{AdmissionPhase, HitlPhase};

impl Publisher {
    pub(super) fn resume(
        store: Arc<A2aStore>,
        session: &NatsSession,
        channels: StreamChannels,
        admission: AdmissionOwnership,
    ) -> Self {
        let mut publisher = Self::new(store, session, channels, admission);
        publisher.resumed = true;
        if let Some(answer) = publisher
            .record()
            .task
            .artifacts
            .as_ref()
            .and_then(|artifacts| {
                artifacts
                    .iter()
                    .find(|artifact| artifact.artifact_id == "answer")
            })
        {
            publisher.output = Output {
                text: super::durable_output::part_text(&answer.parts),
                pending: String::new(),
                sent: true,
            };
        }
        publisher
    }
}

impl Runner {
    /// Attach an already-decided HITL continuation, without admitting a message.
    /// The caller must first persist Following/Working under the exact checkpoint.
    /// This entry point claims released ownership; same-owner reply/recovery paths
    /// should call resume_owned_hitl instead of acquiring a second scoped lease.
    pub async fn resume_hitl_turn(
        self: &Arc<Self>,
        export: &Export,
        owner: &RequestIdentity,
        session: NatsSession,
    ) -> Result<StartTurnResult> {
        self.authorize_session(export, owner, &session).await?;
        let lease = self
            .candidate_lease(session.storage_key())
            .await?
            .ok_or(RunnerError::Busy)?;
        let authority = self.claim_context(&session, lease).await?;
        self.resume_owned_hitl(export, session, authority).await
    }

    pub(super) async fn resume_owned_hitl(
        self: &Arc<Self>,
        export: &Export,
        session: NatsSession,
        authority: OwnedTask,
    ) -> Result<StartTurnResult> {
        let result = self.validate_resume(&session, &authority).await;
        if let Err(error) = result {
            authority
                .release(&self.store, session.storage_key())
                .await?;
            return Err(error);
        }
        let slot = self.slot(&ContextKey::new(export, session.session_id()));
        let active = slot.active.clone().lock_owned().await;
        self.launch_segment(slot, active, session, None, authority)
            .await
    }

    async fn validate_resume(&self, session: &NatsSession, authority: &OwnedTask) -> Result<()> {
        anyhow::ensure!(
            !self.shutting_down.load(Ordering::SeqCst),
            "A2A runner is shutting down"
        );
        let active = authority
            .context
            .document
            .state
            .active
            .as_ref()
            .context("resume task missing")?;
        anyhow::ensure!(
            active.admission.phase == AdmissionPhase::Admitted,
            "resume prompt not admitted"
        );
        anyhow::ensure!(
            active.snapshot.task.status.state == TaskState::Working,
            "resume task not Working"
        );
        let checkpoint = active
            .hitl_checkpoint
            .as_ref()
            .context("resume HITL checkpoint missing")?;
        anyhow::ensure!(
            checkpoint.phase == HitlPhase::Following,
            "resume HITL request not Following"
        );
        session
            .fixed_prompt_handle(&authority.ticket(session.storage_key())?)
            .await?
            .context("resume fixed prompt missing")?;
        Ok(())
    }
}
