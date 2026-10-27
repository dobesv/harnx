use anyhow::{Context, Result};
use std::time::Duration;

use super::target::ReplyTarget;
use crate::{
    runner::{authority::OwnedTask, reply::reply_snapshot, Runner, RunnerError, StartTurnResult},
    store::{context::HitlPhase, StoreError, TaskRecord},
};

pub(super) enum ClaimOutcome {
    Authority(Box<OwnedTask>),
    Snapshot(Box<StartTurnResult>),
}

pub(super) async fn claim_reply_authority_or_retry(
    runner: &Runner,
    target: &ReplyTarget<'_>,
) -> Result<ClaimOutcome> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let lease = loop {
        if let Some(snapshot) = reply_retry_snapshot(runner, target).await? {
            return Ok(ClaimOutcome::Snapshot(Box::new(reply_snapshot(snapshot))));
        }
        if let Some(lease) = runner.candidate_lease(target.session.storage_key()).await? {
            break lease;
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(RunnerError::Busy);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let authority = runner.claim_context(target.session, lease).await?;
    Ok(ClaimOutcome::Authority(Box::new(authority)))
}

pub(super) async fn handle_accept_error(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: OwnedTask,
    error: anyhow::Error,
) -> Result<StartTurnResult> {
    authority
        .release(&runner.store, target.session.storage_key())
        .await?;
    if let Some(snapshot) = reply_retry_snapshot(runner, target).await? {
        return Ok(reply_snapshot(snapshot));
    }
    Err(error)
}

pub(super) async fn reply_retry_snapshot(
    runner: &Runner,
    target: &ReplyTarget<'_>,
) -> Result<Option<TaskRecord>> {
    let Some(saved) = runner
        .store
        .get_reply_dedupe(target.session.storage_key(), &target.message.message_id)
        .await?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        saved.fingerprint == target.fingerprint,
        StoreError::FingerprintMismatch
    );
    if is_context_unresolved(runner, target, &saved.task_id).await? {
        return Ok(None);
    }
    let record = runner
        .store
        .get_task(target.session.storage_key(), &saved.task_id)
        .await?
        .context("reply dedupe task missing")?;
    Ok(Some(record))
}

async fn is_context_unresolved(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    task_id: &str,
) -> Result<bool> {
    let context = runner
        .store
        .read_context(target.session.storage_key())
        .await?;
    let Some(active) = context.and_then(|c| c.document.state.active) else {
        return Ok(false);
    };
    if active.snapshot.task.id != task_id || active.snapshot.task.status.state.is_terminal() {
        return Ok(false);
    }
    let Some(checkpoint) = active.hitl_checkpoint.as_ref() else {
        return Ok(false);
    };
    Ok(checkpoint.phase == HitlPhase::DecisionIntent
        && checkpoint
            .reply
            .as_ref()
            .is_some_and(|r| r.message_id == target.message.message_id))
}
