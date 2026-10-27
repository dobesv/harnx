use a2a_lf::{StreamResponse, TaskState, TaskStatusUpdateEvent};
use anyhow::{Context, Result};
use harnx_runtime::{
    ApprovalRef, HitlDecisionReceipt, NatsSession, PendingHitlApproval, PromptProgress,
};

use crate::{
    hitl::{parse_reply, ReplyParse},
    runner::{authority::OwnedTask, event_map::status, Runner},
    store::context::{ActiveTask, ContextSnapshot, HitlCheckpoint, HitlPhase, ReplyReceipt},
};

pub(super) async fn check_unchanged_parked(runner: &Runner, session: &NatsSession) -> Result<bool> {
    let Some(context) = runner.store.read_context(session.storage_key()).await? else {
        return Ok(false);
    };
    let Some(checkpoint) = is_parked_waiting_checkpoint(&context) else {
        return Ok(false);
    };
    let authority = OwnedTask::ticket_from_context(&context, session.storage_key())?;
    let progress = session.fixed_prompt_progress(&authority).await?;
    let matches_request = matches!(
        progress,
        PromptProgress::InputRequired(p) if p.approval_ref(authority.invocation_id()) == checkpoint.request
    );
    Ok(matches_request)
}

fn is_active_parked_input_required(active: &ActiveTask) -> bool {
    active.cancel.is_none()
        && active.publication.pending.is_none()
        && active.snapshot.task.status.state == TaskState::InputRequired
}

fn is_checkpoint_waiting(checkpoint: &HitlCheckpoint) -> bool {
    checkpoint.phase == HitlPhase::Waiting && checkpoint.reply.is_none()
}

fn is_parked_waiting_checkpoint(context: &ContextSnapshot) -> Option<&HitlCheckpoint> {
    if context.document.owner.is_some() {
        return None;
    }
    let active = context.document.state.active.as_ref()?;
    if !is_active_parked_input_required(active) {
        return None;
    }
    let checkpoint = active.hitl_checkpoint.as_ref()?;
    if !is_checkpoint_waiting(checkpoint) {
        return None;
    }
    Some(checkpoint)
}

pub(super) async fn park_recovered_hitl(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
    pending: &PendingHitlApproval,
) -> Result<()> {
    let request = pending.approval_ref(authority.ticket(session.storage_key())?.invocation_id());
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("recovery task missing")?;
    if is_already_parked_waiting(active, &request) {
        return Ok(());
    }
    authority
        .update(&runner.store, session.storage_key(), |active| {
            active.snapshot.task.status = status(
                TaskState::InputRequired,
                Some(&crate::hitl::render_challenge(pending)),
            );
            active.hitl_checkpoint = Some(HitlCheckpoint {
                request: request.clone(),
                phase: HitlPhase::Waiting,
                reply: None,
                observed_log_seq: pending.request_seq,
            });
            stage_recovered_status(active, pending.request_seq, "input-required");
        })
        .await?;
    authority
        .flush_pending(&runner.store, session.storage_key())
        .await?;
    Ok(())
}

fn is_already_parked_waiting(active: &ActiveTask, request: &ApprovalRef) -> bool {
    if active.snapshot.task.status.state != TaskState::InputRequired {
        return false;
    }
    let Some(checkpoint) = active.hitl_checkpoint.as_ref() else {
        return false;
    };
    checkpoint.request == *request && checkpoint.phase == HitlPhase::Waiting
}

pub(super) async fn follow_recovered_hitl(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
    decision: &HitlDecisionReceipt,
) -> Result<()> {
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("recovery task missing")?;
    if is_already_following(active, decision)? {
        // The typed follower activates only this ticket's original prompt.
        return Ok(());
    }
    authority
        .update(&runner.store, session.storage_key(), |active| {
            apply_following_checkpoint(active, decision);
        })
        .await?;
    authority
        .flush_pending(&runner.store, session.storage_key())
        .await?;
    Ok(())
}

fn is_already_following(active: &ActiveTask, decision: &HitlDecisionReceipt) -> Result<bool> {
    let Some(checkpoint) = &active.hitl_checkpoint else {
        return Ok(false);
    };
    anyhow::ensure!(
        checkpoint.request == decision.request
            || checkpoint.request.request_seq < decision.request.request_seq,
        "HITL decision predates checkpoint or changed identity"
    );
    if checkpoint.request != decision.request {
        return Ok(false);
    }
    if checkpoint.phase != HitlPhase::Following {
        return Ok(false);
    }
    Ok(active.snapshot.task.status.state == TaskState::Working)
}

fn apply_following_checkpoint(active: &mut ActiveTask, decision: &HitlDecisionReceipt) {
    let checkpoint = active
        .hitl_checkpoint
        .get_or_insert_with(|| HitlCheckpoint {
            request: decision.request.clone(),
            phase: HitlPhase::Following,
            reply: None,
            observed_log_seq: decision.decision_seq,
        });
    if checkpoint.request != decision.request {
        *checkpoint = HitlCheckpoint {
            request: decision.request.clone(),
            phase: HitlPhase::Following,
            reply: None,
            observed_log_seq: decision.decision_seq,
        };
    }
    checkpoint.phase = HitlPhase::Following;
    checkpoint.observed_log_seq = decision.decision_seq;
    if let Some(reply) = checkpoint.reply.as_mut() {
        reply.outcome = Some(
            if reply.approved == decision.approved {
                "already_applied"
            } else {
                "conflicting"
            }
            .into(),
        );
    }
    active.snapshot.task.status = status(TaskState::Working, None);
    stage_recovered_status(active, decision.request.request_seq, "working");
}

pub(super) fn stage_recovered_status(active: &mut ActiveTask, request_seq: u64, state: &str) {
    let id = format!("{}:hitl:{request_seq}:{state}", active.snapshot.task.id);
    crate::runner::outbox::stage_event(
        active,
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: active.snapshot.task.id.clone(),
            context_id: active.snapshot.task.context_id.clone(),
            status: active.snapshot.task.status.clone(),
            metadata: None,
        }),
        &id,
    );
}

pub(super) fn saved_reply_note(
    active: &ActiveTask,
    receipt: &ReplyReceipt,
) -> Result<Option<String>> {
    let saved = active
        .snapshot
        .task
        .history
        .as_ref()
        .and_then(|history| history.iter().find(|m| m.message_id == receipt.message_id))
        .context("accepted reply history missing")?;
    anyhow::ensure!(
        crate::store::message_fingerprint(&saved.parts) == receipt.fingerprint,
        "accepted reply history fingerprint mismatch"
    );
    match parse_reply(&saved.parts) {
        ReplyParse::Approve(note) if receipt.approved => Ok(note),
        ReplyParse::Reject(note) if !receipt.approved => Ok(note),
        _ => anyhow::bail!("accepted reply history decision mismatch"),
    }
}
