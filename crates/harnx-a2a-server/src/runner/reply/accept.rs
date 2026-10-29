use crate::store::context::HitlPhase;
use anyhow::{Context, Result};
use harnx_runtime::{HitlDecisionOutcome, PromptProgress};

use super::{advance, intent, target::ReplyTarget};
use crate::{
    runner::{authority::OwnedTask, reply_note::saved_reply_note, Runner},
    store::context::{HitlAdvanceParams, ReplyReceipt},
};

async fn prepare_and_persist_intent(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
) -> Result<bool> {
    authority
        .flush_pending(&runner.store, target.session.storage_key())
        .await?;
    intent::persist_reply_intent(runner, target, authority).await?;
    #[cfg(feature = "fault-injection")]
    runner
        .hooks
        .checkpoint(crate::fault_injection::Boundary::HitlReplyIntentCommitted)
        .await;
    prepare_reply_delivery(runner, target, authority).await
}

async fn deliver_and_evaluate_reply(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
) -> Result<bool> {
    let (receipt, note) = parse_accepted_receipt(authority)?;
    let outcome = target
        .session
        .decide_hitl_approval_for(target.request, receipt.approved, note)
        .await?;
    #[cfg(feature = "fault-injection")]
    runner
        .hooks
        .checkpoint(crate::fault_injection::Boundary::HitlDecisionCommitted)
        .await;
    let ticket = authority.ticket(target.session.storage_key())?;
    let progress = target.session.fixed_prompt_progress(&ticket).await?;
    let evaluation = DecisionEvaluation {
        outcome: &outcome,
        progress: &progress,
    };
    handle_decision_progress(runner, target, authority, evaluation).await
}

pub(super) async fn accept_reply(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
) -> Result<bool> {
    if !prepare_and_persist_intent(runner, target, authority).await? {
        return Ok(false);
    }
    if !deliver_and_evaluate_reply(runner, target, authority).await? {
        return Ok(false);
    }
    authority
        .flush_pending(&runner.store, target.session.storage_key())
        .await?;
    Ok(true)
}

async fn prepare_reply_delivery(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &OwnedTask,
) -> Result<bool> {
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("reply task missing")?;
    let checkpoint = active
        .hitl_checkpoint
        .as_ref()
        .context("reply checkpoint missing")?;
    let receipt = checkpoint.reply.as_ref().context("reply receipt missing")?;
    runner
        .store
        .put_dedupe_mapping(
            crate::store::DedupeMappingKind::Reply,
            target.session.storage_key(),
            crate::store::DedupeMappingTarget::new(
                receipt.message_identity(),
                &active.snapshot.task.id,
            ),
        )
        .await?;
    Ok(
        checkpoint.phase != HitlPhase::Following
            && !active.snapshot.task.status.state.is_terminal(),
    )
}

fn parse_accepted_receipt(authority: &OwnedTask) -> Result<(ReplyReceipt, Option<String>)> {
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("reply task missing")?;
    let checkpoint = active
        .hitl_checkpoint
        .as_ref()
        .context("reply checkpoint missing")?;
    let receipt = checkpoint
        .reply
        .as_ref()
        .context("reply receipt missing")?
        .clone();
    let note = saved_reply_note(active.snapshot.task.history.as_deref(), &receipt)?;
    Ok((receipt, note))
}

struct DecisionEvaluation<'a> {
    outcome: &'a HitlDecisionOutcome,
    progress: &'a PromptProgress,
}

async fn handle_decision_progress(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
    eval: DecisionEvaluation<'_>,
) -> Result<bool> {
    if is_pending_matching_request(eval.progress, target) && is_conflicting_or_stale(eval.outcome) {
        // No proof that this request advanced. Leave exact intent for a retry;
        // never answer a replacement request with this message.
        return Ok(false);
    }
    match eval.outcome {
        HitlDecisionOutcome::Applied | HitlDecisionOutcome::AlreadyApplied { .. } => {
            advance::advance_reply(
                runner,
                target.session,
                authority,
                HitlAdvanceParams {
                    expected_approval: target.request,
                    outcome: eval.outcome,
                },
            )
            .await?;
        }
        HitlDecisionOutcome::Conflicting | HitlDecisionOutcome::Stale => {
            advance::transition_following_unresolved(runner, target, authority, eval.outcome)
                .await?;
        }
    }
    Ok(true)
}

fn is_pending_matching_request(progress: &PromptProgress, target: &ReplyTarget<'_>) -> bool {
    matches!(
        progress,
        PromptProgress::InputRequired(p) if p.approval_ref(&target.request.invocation_id) == *target.request
    )
}

fn is_conflicting_or_stale(outcome: &HitlDecisionOutcome) -> bool {
    matches!(
        outcome,
        HitlDecisionOutcome::Conflicting | HitlDecisionOutcome::Stale
    )
}
