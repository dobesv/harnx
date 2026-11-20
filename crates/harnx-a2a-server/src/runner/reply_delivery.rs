//! Immediate delivery and owner-loss recovery read the same exact saved message.
use anyhow::{Context, Result};
use harnx_runtime::{HitlDecisionOutcome, NatsSession};

use super::{authority::OwnedTask, reply_note::saved_reply_message, Runner};
use crate::{hitl::interaction, store::context::HitlPhase};

pub(super) async fn deliver_saved(
    session: &NatsSession,
    authority: &OwnedTask,
) -> Result<HitlDecisionOutcome> {
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("reply task missing")?;
    let receipt = active
        .hitl_checkpoint
        .as_ref()
        .and_then(|c| c.reply.as_ref())
        .context("reply receipt missing")?;
    let message = saved_reply_message(active.snapshot.task.history.as_deref(), receipt)?;
    let response = interaction::parse_response(session, &receipt.request, &message.parts).await?;
    anyhow::ensure!(
        (response.action != "deny") == receipt.approved,
        "accepted reply history decision mismatch"
    );
    interaction::decide(session, &receipt.request, &message.parts).await
}

/// A validation failure isn't a decision. Keep receipt dedupe durable, but clear
/// the rejected intent so a new message can correct this same pending request.
pub(super) async fn reject_invalid(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
    error: anyhow::Error,
) -> Result<()> {
    if !error.is::<interaction::InvalidReply>() {
        return Err(error);
    }
    let reason = error.to_string();
    authority
        .update(&runner.store, session.storage_key(), |active| {
            let Some(checkpoint) = active.hitl_checkpoint.as_mut() else {
                return;
            };
            let Some(receipt) = checkpoint.reply.take() else {
                return;
            };
            checkpoint.phase = HitlPhase::Waiting;
            interaction::correction(&mut active.snapshot.task.status, &reason);
            let commit = format!(
                "{}:hitl:{}:invalid:{}",
                active.snapshot.task.id, checkpoint.request.request_seq, receipt.message_id
            );
            super::outbox::stage_event(
                active,
                a2a_lf::StreamResponse::StatusUpdate(a2a_lf::TaskStatusUpdateEvent {
                    task_id: active.snapshot.task.id.clone(),
                    context_id: active.snapshot.task.context_id.clone(),
                    status: active.snapshot.task.status.clone(),
                    metadata: None,
                }),
                &commit,
            );
        })
        .await?;
    authority
        .flush_pending(&runner.store, session.storage_key())
        .await?;
    Ok(())
}
