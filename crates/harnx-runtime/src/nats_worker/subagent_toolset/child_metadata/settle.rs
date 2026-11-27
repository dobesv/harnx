//! Establishing the guard and applying the patch for a patch-bearing call.

use super::admission::{admitted_before, apply_pinned};
use super::guards::{insert_guard, pin_guard, stable_invocation, MetadataGuard};
use super::intent::{patch_intent, ExistingPin, PatchIntent};
use super::{storage_key, ChildCreation, MetadataPlan, SubagentToolset};
use crate::nats_session_metadata::ChildSessionMetadataPatch;
use harnx_toolset::{ToolInvocationContext, ToolInvokeError};
use serde_json::{json, Value};

/// The resolved target of a patch-bearing call's guard/mutation phase.
struct SettleTarget<'a> {
    session_id: &'a str,
    storage: String,
    call_id: &'a str,
    patch: ChildSessionMetadataPatch,
}

/// Establish the guard and apply the patch for a patch-bearing call with a
/// resolved target. `None` target is a generated-ID create: the reservation
/// stamp is the guard, no checkpoint protocol is added.
async fn settle(
    toolset: &SubagentToolset,
    context: &ToolInvocationContext,
    target: Option<&str>,
    patch: ChildSessionMetadataPatch,
    guard_checkpoint: impl FnOnce(&str, MetadataGuard) -> Value,
) -> Result<MetadataPlan, ToolInvokeError> {
    let call_id = stable_invocation(context)?;
    let Some(session_id) = target else {
        return Ok(MetadataPlan::Create(Box::new(ChildCreation::new(
            patch, call_id,
        ))));
    };
    let target = SettleTarget {
        session_id,
        storage: storage_key(toolset, session_id),
        call_id,
        patch,
    };
    if admitted_before(toolset, &target.storage, call_id).await? {
        return Ok(MetadataPlan::Settled);
    }
    let checkpoint = context.checkpoint.as_ref();
    match patch_intent(toolset, &target.storage, call_id, checkpoint).await? {
        PatchIntent::Bypass => Ok(MetadataPlan::Settled),
        PatchIntent::Create { pinned } => {
            settle_create(context, &target, pinned, guard_checkpoint).await
        }
        PatchIntent::Existing(pin) => {
            settle_existing(toolset, context, target, pin, guard_checkpoint).await
        }
        PatchIntent::CompetingCreation => Err(ToolInvokeError::Recoverable(format!(
            "session '{}' was created by a competing invocation; the requested metadata \
             update conflicts",
            target.session_id
        ))),
        PatchIntent::GuardWithoutRecord => Err(ToolInvokeError::Recoverable(format!(
            "session '{}' no longer has a metadata record; its pinned metadata revision \
             cannot be applied",
            target.session_id
        ))),
        PatchIntent::LegacyCheckpoint => Err(ToolInvokeError::Recoverable(
            "metadata-bearing recovery found an invocation checkpoint without a revision \
             guard; refusing to retrofit one"
                .into(),
        )),
    }
}

async fn settle_create(
    context: &ToolInvocationContext,
    target: &SettleTarget<'_>,
    pinned: bool,
    guard_checkpoint: impl FnOnce(&str, MetadataGuard) -> Value,
) -> Result<MetadataPlan, ToolInvokeError> {
    if !pinned {
        pin_guard(
            context,
            guard_checkpoint(target.session_id, MetadataGuard::Creation),
            MetadataGuard::Creation,
            target.session_id,
        )
        .await?;
    }
    Ok(MetadataPlan::Create(Box::new(ChildCreation::new(
        target.patch.clone(),
        target.call_id,
    ))))
}

async fn settle_existing(
    toolset: &SubagentToolset,
    context: &ToolInvocationContext,
    target: SettleTarget<'_>,
    pin: ExistingPin,
    guard_checkpoint: impl FnOnce(&str, MetadataGuard) -> Value,
) -> Result<MetadataPlan, ToolInvokeError> {
    let revision = match pin {
        ExistingPin::Pinned(revision) => revision,
        ExistingPin::Fresh(revision) => {
            pinned_revision(context, &target, revision, guard_checkpoint).await?
        }
    };
    apply_pinned(
        toolset,
        &target.storage,
        target.call_id,
        &target.patch,
        revision,
    )
    .await?;
    Ok(MetadataPlan::Settled)
}

/// Pin the revision guard in the first-writer checkpoint, honoring a pin a
/// concurrent attempt of the same call already wrote.
async fn pinned_revision(
    context: &ToolInvocationContext,
    target: &SettleTarget<'_>,
    revision: u64,
    guard_checkpoint: impl FnOnce(&str, MetadataGuard) -> Value,
) -> Result<u64, ToolInvokeError> {
    let stored = pin_guard(
        context,
        guard_checkpoint(target.session_id, MetadataGuard::Revision(revision)),
        MetadataGuard::Revision(revision),
        target.session_id,
    )
    .await?;
    match stored {
        MetadataGuard::Revision(revision) => Ok(revision),
        // pin_guard rejects incompatible kinds, so this arm only guards
        // against a future regression in its kind check.
        MetadataGuard::Creation => Err(ToolInvokeError::Fatal(
            "sub-agent metadata checkpoint guard disagrees with the pinned kind".into(),
        )),
    }
}

/// The sync (session_new/session_prompt) guard checkpoint: the same fields
/// `checkpointed_session` checkpoints, plus the guard.
pub(in crate::nats_worker::subagent_toolset) async fn prepare_sync(
    toolset: &SubagentToolset,
    context: &ToolInvocationContext,
    target: Option<&str>,
    patch: ChildSessionMetadataPatch,
) -> Result<MetadataPlan, ToolInvokeError> {
    // The saved invocation checkpoint must match this worker's route scope
    // before the gate can pin a guard or mutate (review F03, synchronous
    // path). Metadata-free and admitted calls never reach this point.
    if let Some(saved) = context.checkpoint.as_ref() {
        super::validate_sync_saved_scope(toolset, saved)?;
    }
    let cluster = toolset.route.cluster().to_string();
    let toolset_agent = toolset.agent.clone();
    settle(toolset, context, target, patch, |session_id, guard| {
        let mut value = json!({
            "session_id": session_id,
            "storage_key": harnx_core::session_identity::session_key(Some(&toolset_agent), session_id),
            "cluster": cluster,
        });
        insert_guard(&mut value, guard);
        value
    })
    .await
}

/// The async (session_send) guard checkpoint: the full dispatch checkpoint
/// fields, plus the guard.
pub(in crate::nats_worker::subagent_toolset) async fn prepare_send(
    toolset: &SubagentToolset,
    context: &ToolInvocationContext,
    target: Option<&str>,
    parent_key: &str,
    patch: ChildSessionMetadataPatch,
) -> Result<MetadataPlan, ToolInvokeError> {
    let cluster = toolset.route.cluster().to_string();
    let toolset_agent = toolset.agent.clone();
    let call_id = context.call_id.clone();
    let parent_key = parent_key.to_string();
    settle(toolset, context, target, patch, |session_id, guard| {
        let mut value = json!({
            "session_id": session_id,
            "storage_key": harnx_core::session_identity::session_key(Some(&toolset_agent), session_id),
            "cluster": cluster,
            "prompt_id": call_id,
            "parent_storage_key": parent_key,
        });
        insert_guard(&mut value, guard);
        value
    })
    .await
}
