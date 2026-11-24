use anyhow::{Context, Result};
use harnx_runtime::{ApprovalRef, NatsSession, PendingHitlApproval, PromptProgress};

use super::{hitl::RecoveryAction, hitl_state};
use crate::{
    runner::{authority::OwnedTask, Runner},
    store::context::HitlPhase,
};

async fn reconcile_pending_request_checkpoint(
    session: &NatsSession,
    checkpoint: &crate::store::context::HitlCheckpoint,
    request: &ApprovalRef,
) -> Result<bool> {
    anyhow::ensure!(
        checkpoint.request == *request
            || checkpoint.request.request_seq < request.request_seq
            || crate::hitl::interaction::is_manifest_successor(
                session,
                &checkpoint.request,
                request
            )
            .await?,
        "HITL runtime request regressed or changed identity"
    );
    if checkpoint.request == *request {
        anyhow::ensure!(
            checkpoint.phase == HitlPhase::DecisionIntent || checkpoint.phase == HitlPhase::Waiting,
            "HITL Following request still undecided"
        );
        Ok(checkpoint.reply.is_some())
    } else {
        Ok(false)
    }
}

pub(super) async fn evaluate_input_required_progress(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
    pending: &PendingHitlApproval,
) -> Result<Option<RecoveryAction>> {
    let invocation_id = authority
        .ticket(session.storage_key())?
        .invocation_id()
        .to_string();
    let request = pending.approval_ref(&invocation_id);
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("recovery task missing")?;
    if let Some(checkpoint) = &active.hitl_checkpoint {
        if reconcile_pending_request_checkpoint(session, checkpoint, &request).await? {
            match crate::runner::reply_delivery::deliver_saved(session, authority).await {
                Ok(_) => return Ok(None),
                Err(error) => {
                    crate::runner::reply_delivery::reject_invalid(
                        runner, session, authority, error,
                    )
                    .await?;
                    return Ok(Some(RecoveryAction::Parked));
                }
            }
        }
    }
    hitl_state::park_recovered_hitl(runner, session, authority, pending).await?;
    Ok(Some(RecoveryAction::Parked))
}

pub(super) fn should_settle_early(authority: &OwnedTask) -> bool {
    authority
        .context
        .document
        .state
        .active
        .as_ref()
        .is_none_or(|a| a.cancel.is_some() || a.snapshot.task.status.state.is_terminal())
}

pub(super) fn ensure_no_hitl_checkpoint(authority: &OwnedTask, reason: &str) -> Result<()> {
    anyhow::ensure!(
        authority
            .context
            .document
            .state
            .active
            .as_ref()
            .is_some_and(|a| a.hitl_checkpoint.is_none()),
        "{reason}"
    );
    Ok(())
}

pub(super) async fn classify_recovered_progress(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
    progress: PromptProgress,
) -> Result<Option<RecoveryAction>> {
    match progress {
        PromptProgress::Terminal(_) => Ok(Some(RecoveryAction::Settle)),
        PromptProgress::InputRequired(pending) => {
            evaluate_input_required_progress(runner, session, authority, &pending).await
        }
        PromptProgress::Parked => recover_interaction_progress(runner, session, authority).await,
        PromptProgress::Running {
            last_decision: Some(decision),
        } => {
            hitl_state::follow_recovered_hitl(runner, session, authority, &decision).await?;
            Ok(Some(RecoveryAction::Follow))
        }
        PromptProgress::Running {
            last_decision: None,
        } => {
            if recover_interaction_progress(runner, session, authority).await?
                == Some(RecoveryAction::Follow)
            {
                return Ok(Some(RecoveryAction::Follow));
            }
            ensure_no_hitl_checkpoint(
                authority,
                "HITL checkpoint has no runtime request or decision",
            )?;
            Ok(Some(RecoveryAction::Settle))
        }
    }
}

async fn recover_interaction_progress(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
) -> Result<Option<RecoveryAction>> {
    let checkpoint = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .and_then(|a| a.hitl_checkpoint.as_ref());
    if let Some(checkpoint) = checkpoint {
        if let Some(decision) =
            crate::hitl::interaction::accepted_decision(session, &checkpoint.request).await?
        {
            hitl_state::follow_recovered_hitl(runner, session, authority, &decision).await?;
            return Ok(Some(RecoveryAction::Follow));
        }
    }
    // Round-start-only and handoff-owned work still need a newer worker.
    Ok(Some(RecoveryAction::Parked))
}
