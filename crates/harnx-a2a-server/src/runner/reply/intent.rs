use a2a_lf::TaskState;
use anyhow::{Context, Result};

use super::target::ReplyTarget;
use crate::{
    hitl::{parse_reply, ReplyParse},
    runner::{authority::OwnedTask, Runner, RunnerError},
    store::{
        context::{
            AuthorityError, ContextSnapshot, ContextState, ContextVersion, HitlPhase, ReplyReceipt,
            ReplyReceiptParams,
        },
        StoreError,
    },
};

async fn read_validated_context(
    runner: &Runner,
    storage_key: &str,
    authority: &OwnedTask,
) -> Result<ContextSnapshot> {
    let expected = authority.context.version()?;
    anyhow::ensure!(
        authority.lease.revalidate_ownership().await?,
        "A2A scoped lease lost"
    );
    let current = runner
        .store
        .read_context(storage_key)
        .await?
        .context("reply context missing")?;
    validate_context_ownership(&current.version()?, &expected)?;
    Ok(current)
}

async fn try_persist_intent_attempt(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
    receipt: &ReplyReceipt,
) -> Result<bool> {
    let current = read_validated_context(runner, target.session.storage_key(), authority).await?;
    if check_existing_intent(&current, target, authority)? {
        return Ok(true);
    }
    let version = current.version()?;
    let commit = IntentCommit {
        receipt,
        version: &version,
    };
    commit_intent_update(runner, target, authority, commit).await
}

pub(super) async fn persist_reply_intent(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
) -> Result<()> {
    let receipt = build_reply_receipt(target)?;
    for _ in 0..8 {
        if try_persist_intent_attempt(runner, target, authority, &receipt).await? {
            return Ok(());
        }
    }
    anyhow::bail!(AuthorityError::Conflict)
}

fn build_reply_receipt(target: &ReplyTarget<'_>) -> Result<ReplyReceipt> {
    let (approved, note) = match parse_reply(&target.message.parts) {
        ReplyParse::Approve(note) => (true, note),
        ReplyParse::Reject(note) => (false, note),
        ReplyParse::Unrecognized => anyhow::bail!("reply not recognized"),
    };
    Ok(ReplyReceipt::new(ReplyReceiptParams {
        message_id: target.message.message_id.clone(),
        fingerprint: target.fingerprint.into(),
        request: target.request.clone(),
        approved,
        note,
    }))
}

fn validate_context_ownership(version: &ContextVersion, expected: &ContextVersion) -> Result<()> {
    anyhow::ensure!(
        version.owner == expected.owner && version.task_id == expected.task_id,
        AuthorityError::StaleOwner
    );
    Ok(())
}

fn check_existing_intent(
    current: &ContextSnapshot,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
) -> Result<bool> {
    let active = current
        .document
        .state
        .active
        .as_ref()
        .context("reply task missing")?;
    let checkpoint = active
        .hitl_checkpoint
        .as_ref()
        .context("reply checkpoint missing")?;
    anyhow::ensure!(
        checkpoint.request == *target.request,
        AuthorityError::Conflict
    );
    let Some(saved) = &checkpoint.reply else {
        return Ok(false);
    };
    anyhow::ensure!(
        saved.message_id == target.message.message_id,
        AuthorityError::Conflict
    );
    anyhow::ensure!(
        saved.fingerprint == target.fingerprint,
        StoreError::FingerprintMismatch
    );
    authority.context = current.clone();
    Ok(true)
}

struct IntentCommit<'a> {
    receipt: &'a ReplyReceipt,
    version: &'a ContextVersion,
}

async fn commit_intent_update(
    runner: &Runner,
    target: &ReplyTarget<'_>,
    authority: &mut OwnedTask,
    commit: IntentCommit<'_>,
) -> Result<bool> {
    let prepared = runner
        .store
        .prepare_context_update_checked(
            crate::store::context::ContextMutation {
                storage_key: target.session.storage_key(),
                expected: commit.version,
                operation_id: &uuid::Uuid::new_v4().to_string(),
            },
            |state| mutate_context_intent(state, target, commit.receipt),
        )
        .await;
    let committed = match prepared {
        Ok(write) => runner.store.commit_context(&write).await,
        Err(error) => Err(error),
    };
    match committed {
        Ok(context) => {
            authority.context = context;
            Ok(true)
        }
        Err(error) if error.downcast_ref::<AuthorityError>() == Some(&AuthorityError::Conflict) => {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn mutate_context_intent(
    state: &mut ContextState,
    target: &ReplyTarget<'_>,
    receipt: &ReplyReceipt,
) -> Result<()> {
    let active = state.active.as_mut().context("reply task missing")?;
    anyhow::ensure!(active.cancel.is_none(), RunnerError::Busy);
    anyhow::ensure!(
        active.snapshot.task.status.state == TaskState::InputRequired,
        AuthorityError::Conflict
    );
    let checkpoint = active
        .hitl_checkpoint
        .as_mut()
        .context("reply checkpoint missing")?;
    anyhow::ensure!(
        checkpoint.request == *target.request
            && checkpoint.phase == HitlPhase::Waiting
            && checkpoint.reply.is_none(),
        AuthorityError::Conflict
    );
    checkpoint.phase = HitlPhase::DecisionIntent;
    checkpoint.reply = Some(receipt.clone());
    active
        .snapshot
        .task
        .history
        .get_or_insert_with(Vec::new)
        .push(target.message.clone());
    Ok(())
}
