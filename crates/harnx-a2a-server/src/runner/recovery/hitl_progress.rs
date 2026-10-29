use anyhow::{Context, Result};
use harnx_runtime::{ApprovalRef, NatsSession, PendingHitlApproval, PromptProgress};

use super::{hitl::RecoveryAction, hitl_state};
use crate::{
    runner::{authority::OwnedTask, Runner},
    store::context::HitlPhase,
};

async fn redeliver_saved_decision_if_pending(
    session: &NatsSession,
    active: &crate::store::context::ActiveTask,
    checkpoint: &crate::store::context::HitlCheckpoint,
) -> Result<bool> {
    if let Some(receipt) = &checkpoint.reply {
        anyhow::ensure!(
            checkpoint.phase == HitlPhase::DecisionIntent,
            "accepted HITL request is not DecisionIntent"
        );
        let note = crate::runner::reply_note::saved_reply_note(
            active.snapshot.task.history.as_deref(),
            receipt,
        )?;
        session
            .decide_hitl_approval_for(&receipt.request, receipt.approved, note)
            .await?;
        Ok(true)
    } else {
        anyhow::ensure!(
            checkpoint.phase == HitlPhase::Waiting,
            "HITL Following request still undecided"
        );
        Ok(false)
    }
}

async fn reconcile_pending_request_checkpoint(
    session: &NatsSession,
    active: &crate::store::context::ActiveTask,
    checkpoint: &crate::store::context::HitlCheckpoint,
    request: &ApprovalRef,
) -> Result<bool> {
    anyhow::ensure!(
        checkpoint.request == *request || checkpoint.request.request_seq < request.request_seq,
        "HITL runtime request regressed or changed identity"
    );
    if checkpoint.request == *request {
        redeliver_saved_decision_if_pending(session, active, checkpoint).await
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
        if reconcile_pending_request_checkpoint(session, active, checkpoint, &request).await? {
            return Ok(None);
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
        PromptProgress::Running {
            last_decision: Some(decision),
        } => {
            hitl_state::follow_recovered_hitl(runner, session, authority, &decision).await?;
            Ok(Some(RecoveryAction::Follow))
        }
        PromptProgress::Running {
            last_decision: None,
        } => {
            ensure_no_hitl_checkpoint(
                authority,
                "HITL checkpoint has no runtime request or decision",
            )?;
            Ok(Some(RecoveryAction::Settle))
        }
    }
}
