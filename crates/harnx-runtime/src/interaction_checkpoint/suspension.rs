use super::{live::DurableRound, snapshot::CallCheckpoint};
use anyhow::{ensure, Context, Result};
use harnx_core::{interaction::InteractionResponse, session::SessionLogEntry};
use harnx_engine::tool::{
    PendingToolInteraction, ToolEvalContext, ToolRoundSuspension, ToolUseConfirmation,
};

impl DurableRound {
    pub(super) async fn suspend(&self, mut suspension: ToolRoundSuspension) -> Result<()> {
        let mut pending = Vec::new();
        for call in &mut suspension.pending {
            ensure!(
                call.interaction.chain_version == self.chain_version,
                "suspended middleware version mismatch"
            );
            if self.pending_reference(call).await?.is_none() {
                let checkpoint = self.store_call_checkpoint(call).await?;
                self.append_interaction_request(call, checkpoint).await?;
            }
            pending.push(call.interaction.interaction_id.clone());
        }
        crate::fault_injection::hit(
            self.session.backend.session_id(),
            crate::fault_injection::RecoveryFaultPoint::SuspensionReadyBefore,
        )
        .await;
        self.session
            .append(&SessionLogEntry::SuspensionReady {
                tool_round_seq: self.seq,
                pending,
                fence_token: self.session.lease.fence_token(),
            })
            .await?;
        crate::fault_injection::hit(
            self.session.backend.session_id(),
            crate::fault_injection::RecoveryFaultPoint::SuspensionReadyAfter,
        )
        .await;
        Ok(())
    }

    async fn store_call_checkpoint(&self, call: &mut PendingToolInteraction) -> Result<String> {
        for snapshot in call.checkpoint.snapshots.values_mut() {
            self.externalize(&mut snapshot.result).await?;
        }
        ensure!(
            self.session.lease.revalidate_ownership().await?,
            "session lease lost before checkpoint"
        );
        crate::fault_injection::hit(
            self.session.backend.session_id(),
            crate::fault_injection::RecoveryFaultPoint::CheckpointBefore,
        )
        .await;
        let checkpoint = self
            .store
            .put(
                &self.prefix(&call.call)?,
                &format!("checkpoints.{}", uuid::Uuid::new_v4()),
                &CallCheckpoint::new(&call.checkpoint),
            )
            .await?;
        crate::fault_injection::hit(
            self.session.backend.session_id(),
            crate::fault_injection::RecoveryFaultPoint::CheckpointAfter,
        )
        .await;
        Ok(checkpoint)
    }

    async fn append_interaction_request(
        &self,
        call: &PendingToolInteraction,
        checkpoint: String,
    ) -> Result<()> {
        let request = &call.interaction;
        crate::fault_injection::hit(
            self.session.backend.session_id(),
            crate::fault_injection::RecoveryFaultPoint::InteractionRequestedBefore,
        )
        .await;
        self.session
            .append(&SessionLogEntry::InteractionRequested {
                tool_round_seq: self.seq,
                tool_call_id: call.call.id.clone().context("missing suspended call ID")?,
                interaction_id: request.interaction_id.clone(),
                producer: request.producer.clone(),
                step_path: request.step_path.clone(),
                key: request.key.clone(),
                kind: request.kind.clone(),
                payload: request.payload.clone(),
                summary: request.summary.clone(),
                chain_version: request.chain_version.clone(),
                checkpoint,
                fence_token: self.session.lease.fence_token(),
            })
            .await?;
        crate::fault_injection::hit(
            self.session.backend.session_id(),
            crate::fault_injection::RecoveryFaultPoint::InteractionRequestedAfter,
        )
        .await;
        Ok(())
    }

    pub(super) async fn answer_live(
        &self,
        ctx: &ToolEvalContext,
        suspension: &ToolRoundSuspension,
    ) -> Result<bool> {
        let mut answered = false;
        for pending in &suspension.pending {
            if pending.interaction.kind != "confirm" {
                continue;
            }
            let decision = (ctx.confirm_tool_use_fn)(
                &pending.call,
                &pending.call.arguments,
                Some(&pending.interaction.summary),
            );
            let (action, note) = match decision {
                ToolUseConfirmation::Approve => ("approve", None),
                ToolUseConfirmation::Deny { reason } => ("deny", reason),
                ToolUseConfirmation::Defer => continue,
            };
            let entries = self.session.backend.load_events_latest_async().await?;
            let request_seq = entries
                .iter()
                .rev()
                .find_map(|(seq, entry)| match entry {
                    SessionLogEntry::InteractionRequested { interaction_id, .. }
                        if interaction_id == &pending.interaction.interaction_id =>
                    {
                        Some(*seq)
                    }
                    _ => None,
                })
                .context("live interaction has no durable request")?;
            let outcome = crate::interactions::accept_interaction_response(
                &self.session.backend,
                &self.session.lease,
                &pending.interaction.interaction_id,
                request_seq,
                &InteractionResponse {
                    action: action.into(),
                    data: Default::default(),
                    note,
                },
            )
            .await?;
            ensure!(
                outcome.is_applied(),
                "live interaction decision rejected: {outcome:?}"
            );
            answered = true;
        }
        Ok(answered)
    }
}
