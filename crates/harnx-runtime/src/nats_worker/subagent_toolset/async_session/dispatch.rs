//! Send admission and delivery only. Observation and discovery are separate.
use super::super::child_metadata::{self, MetadataPlan};
use super::super::{normalize_session_id, parse_attachment_urls, SubagentToolset};
use super::{
    parse_send_args, DispatchConfirmation, PromptHandle, ResolvedWait, SessionPromptResult,
    SessionPromptStatus,
};
use crate::nats_session::{dispatch::PromptActivation, AppendedPrompt, NatsSession};
use crate::nats_session_metadata::{InvocationEdgeKind, RunLimitsRecord};
use harnx_toolset::{ToolInvocationContext, ToolInvokeError};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub(in crate::nats_worker::subagent_toolset) struct DispatchedPrompt {
    pub(in crate::nats_worker::subagent_toolset) handle: PromptHandle,
    pub(in crate::nats_worker::subagent_toolset) prompt: AppendedPrompt,
    pub(super) dispatch: Option<DispatchConfirmation>,
}

/// The only parent authority accepted by async dispatch/observation. Hidden
/// arguments can label a transcript call but cannot supply storage scope.
pub(in crate::nats_worker::subagent_toolset) fn trusted_parent_scope(
    context: &ToolInvocationContext,
) -> Result<String, ToolInvokeError> {
    let from_identity = context.invoking_session.as_ref().map(|session| {
        harnx_core::session_identity::session_key(session.agent.as_deref(), &session.session_id)
    });
    let from_transport = context
        .invoking_session_id
        .as_ref()
        .filter(|key| !key.trim().is_empty());
    if let (Some(identity), Some(transport)) = (&from_identity, from_transport) {
        if identity != transport {
            return Err(ToolInvokeError::Fatal(
                "trusted parent session identity disagrees with storage scope".into(),
            ));
        }
    }
    from_identity
        .or_else(|| from_transport.cloned())
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            ToolInvokeError::Fatal(
                "async sub-agent dispatch requires trusted parent storage scope".into(),
            )
        })
}

/// The metadata gate for a send, with its fresh-content validation. The
/// gate runs before the checkpointed create and before any fresh
/// attachment composition: a recoverable conflict or a failed guard must
/// dispatch nothing, and an admitted replay must not reapply its patch
/// over later child edits.
// qual:allow(iosp) — the gate interleaves guard-state logic with broker calls by design: the fresh-compose check, the composition and the settle step share one target/call context, and each depends on the previous step's durable outcome.
async fn send_metadata_plan(
    toolset: &SubagentToolset,
    args: &super::SendArgs,
    context: &ToolInvocationContext,
    inherited: &InheritedDispatch,
    patch: Option<crate::nats_session_metadata::ChildSessionMetadataPatch>,
) -> Result<MetadataPlan, ToolInvokeError> {
    let Some(patch) = patch else {
        return Ok(MetadataPlan::Settled);
    };
    // The saved checkpoint's scope is validated before the gate can pin a
    // guard or CAS (review F03): a same-broker route/cluster change can keep
    // the journaled request, the child record and its revision valid while
    // the saved scope is stale. Patchless sends keep their existing later
    // validation in `checkpointed_session`.
    if let Some(saved) = context.checkpoint.as_ref() {
        validate_checkpoint(saved, toolset, &inherited.parent_key, &context.call_id)?;
    }
    let target = child_metadata::checkpoint_session_id(context)
        .or_else(|| normalize_session_id(args.session_id.clone()));
    // Fresh or candidate-only calls validate their original content BEFORE
    // the gate can mutate or create: a missing attachment must fail without
    // touching the child. Admitted replays deliver durable content and
    // skip this re-fetch.
    if child_metadata::fresh_send_composes_content(toolset, target.as_deref(), &context.call_id)
        .await?
    {
        toolset
            .compose_prompt_content(
                args.message.clone(),
                args.attachments.clone().unwrap_or_default(),
            )
            .await?;
    }
    child_metadata::prepare_send(
        toolset,
        context,
        target.as_deref(),
        &inherited.parent_key,
        patch,
    )
    .await
}

/// Resolve the content a dispatched send delivers. Replayed delivery never
/// re-fetches attachments or replaces original content, even if the
/// referenced resource disappeared after admission. A joined-dispatch
/// record is authoritative here, including over a leftover candidate
/// intent from an earlier busy reserve (issue #2672).
// qual:allow(iosp) — the joined/admission/fresh fallback chain is ordered broker lookups whose branch conditions are the lookups' results; splitting the logic from the calls would re-fetch or reorder admission state.
async fn replayed_or_fresh_content(
    toolset: &SubagentToolset,
    session: &NatsSession,
    args: &super::SendArgs,
    inherited: &InheritedDispatch,
    call_id: &str,
) -> Result<harnx_core::message::MessageContent, ToolInvokeError> {
    match session
        .metadata_store()
        .joined_dispatch(session.storage_key(), call_id)
        .await
        .map_err(recoverable)?
    {
        Some(joined) => {
            ensure_joined_caller(&joined, &inherited.parent_key, &inherited.parent)?;
            Ok(joined.prompt_content)
        }
        None => match toolset
            .session_metadata
            .admission(session.storage_key(), call_id)
            .await
            .map_err(recoverable)?
        {
            Some(saved) => saved.prompt_content.ok_or_else(|| {
                ToolInvokeError::Fatal("dispatch admission has no original content".into())
            }),
            None => {
                toolset
                    .compose_prompt_content(
                        args.message.clone(),
                        args.attachments.clone().unwrap_or_default(),
                    )
                    .await
            }
        },
    }
}

/// Returns after durable append and one bounded activation publication. No
/// follower, detached future, progress supervisor or cancellation-on-drop.
pub(in crate::nats_worker::subagent_toolset) async fn dispatch_send(
    toolset: &SubagentToolset,
    arguments: Value,
    cancel: CancellationToken,
    context: ToolInvocationContext,
) -> Result<(DispatchedPrompt, Option<ResolvedWait>), ToolInvokeError> {
    let (args, wait) = parse_send_args(arguments).map_err(ToolInvokeError::Recoverable)?;
    let inherited = validate_send_context(&args, &context)?;
    let patch = child_metadata::extract(args.metadata.clone())?;
    let budget = super::RESULT_MAX_BYTES;
    let requested_id = normalize_session_id(args.session_id.clone());
    // New IDs are six ASCII bytes. Include the diagnostic invocation identity
    // in preflight; Task1's handle-only reservation intentionally omits it.
    ensure_receipt_fits(
        requested_id.as_deref().unwrap_or("000000"),
        &context.call_id,
        budget,
    )?;
    check_cancel(&cancel)?;
    let plan = send_metadata_plan(toolset, &args, &context, &inherited, patch).await?;
    let session = checkpointed_session(
        toolset,
        &args,
        &inherited.parent_key,
        &context,
        plan.creation(),
    )
    .await?;
    ensure_receipt_fits(session.session_id(), &context.call_id, budget)?;
    let session = inherited.admit_detached(session, &context, args.timeout_secs);
    let content =
        replayed_or_fresh_content(toolset, &session, &args, &inherited, &context.call_id).await?;
    check_cancel(&cancel)?;
    record_child(toolset, &context, session.session_id()).await?;
    // Parent interruption and observer cancellation do not stop detached work.
    // Explicit session_cancel and inherited hard deadlines still stop the child.
    let prompt = session
        .append_dispatch_content(content)
        .await
        .map_err(recoverable)?;
    Ok((DispatchedPrompt::activate(&session, prompt).await, wait))
}

impl DispatchedPrompt {
    async fn activate(session: &NatsSession, prompt: AppendedPrompt) -> Self {
        // A publication failure is downgraded to `unconfirmed`: the durable
        // handle must survive it, and a later wait/load/repair can still
        // confirm or publish the activation.
        let dispatch = match session.activate_dispatch_prompt(&prompt).await {
            Ok(PromptActivation::Published) => Some(DispatchConfirmation::Confirmed),
            Ok(PromptActivation::NotNeeded) => None,
            Err(_) => Some(DispatchConfirmation::Unconfirmed),
        };
        Self {
            handle: PromptHandle {
                session_id: session.session_id().into(),
                prompt_id: prompt.user_msg_id().into(),
            },
            prompt,
            dispatch,
        }
    }
}

struct InheritedDispatch {
    parent_key: String,
    parent: RunLimitsRecord,
    admitted_at: chrono::DateTime<chrono::Utc>,
}

impl InheritedDispatch {
    fn admit_detached(
        &self,
        session: NatsSession,
        context: &ToolInvocationContext,
        timeout_secs: Option<u64>,
    ) -> NatsSession {
        session
            .with_inherited_admission(
                self.parent.clone(),
                context.call_id.clone(),
                self.admitted_at,
                InvocationEdgeKind::Delegation,
                timeout_secs,
            )
            .with_detached_admission()
            .with_execution_parent(self.parent_key.clone(), context.call_id.clone())
    }
}

/// A joined-dispatch record is the caller's immutable dispatch identity. The
/// same parent fields the invocation-level check compares must match before
/// the record's content can be replayed to this caller. One shared predicate
/// with the other replay/activation sites (issue #2672, review F7).
fn ensure_joined_caller(
    joined: &crate::nats_session_metadata::JoinedDispatch,
    parent_key: &str,
    parent: &RunLimitsRecord,
) -> Result<(), ToolInvokeError> {
    if joined.matches_caller(Some(parent_key), Some(parent.invocation_id.as_str())) {
        Ok(())
    } else {
        Err(ToolInvokeError::Fatal(
            "dispatch admission parent scope mismatch".into(),
        ))
    }
}

fn validate_send_context(
    args: &super::SendArgs,
    context: &ToolInvocationContext,
) -> Result<InheritedDispatch, ToolInvokeError> {
    if args.message.trim().is_empty() {
        return Err(ToolInvokeError::Recoverable(
            "message must not be empty".into(),
        ));
    }
    let parent_key = trusted_parent_scope(context)?;
    if args
        .parent_session_id
        .as_ref()
        .is_some_and(|arg| arg != &parent_key)
    {
        return Err(ToolInvokeError::Recoverable(
            "parent session argument disagrees with trusted storage scope".into(),
        ));
    }
    if context.call_id.is_empty() {
        return Err(ToolInvokeError::Fatal(
            "async dispatch requires stable invocation identity".into(),
        ));
    }
    // Optional stores are no-ops for legacy tools, but send promises recovery
    // even when its generated session ID and tool reply never reach the caller.
    context.checkpoint_store.as_ref().ok_or_else(|| {
        ToolInvokeError::Fatal("async dispatch requires durable checkpoint store".into())
    })?;
    context.partial_result_store.as_ref().ok_or_else(|| {
        ToolInvokeError::Fatal("async dispatch requires durable partial-result store".into())
    })?;
    parse_attachment_urls(args.attachments.clone().unwrap_or_default())?;
    // Same trusted-lineage validation the synchronous path runs up front
    // (review F02); one implementation, so a direct/MCP dispatch can never
    // drift from what `run_prompt` accepts.
    let inherited = child_metadata::InheritedPrompt::validate(context)?;
    Ok(InheritedDispatch {
        parent_key,
        parent: inherited.parent,
        admitted_at: inherited.admitted_at,
    })
}

fn ensure_receipt_fits(
    session_id: &str,
    prompt_id: &str,
    budget: usize,
) -> Result<(), ToolInvokeError> {
    let receipt = SessionPromptResult {
        invocation_id: Some(prompt_id.into()),
        dispatch: Some(DispatchConfirmation::Unconfirmed),
        status: SessionPromptStatus::Cancelled,
        elapsed_ms: u64::MAX,
        ..SessionPromptResult::empty(&super::PromptHandle {
            session_id: session_id.into(),
            prompt_id: prompt_id.into(),
        })
    };
    let size = serde_json::to_vec(&receipt)
        .map_err(|error| ToolInvokeError::Fatal(error.to_string()))?
        .len();
    if size > budget {
        return Err(ToolInvokeError::Recoverable(format!(
            "dispatch result metadata ({size} bytes) exceeds the {budget} byte result budget"
        )));
    }
    Ok(())
}

async fn checkpointed_session(
    toolset: &SubagentToolset,
    args: &super::SendArgs,
    parent: &str,
    context: &ToolInvocationContext,
    creation: Option<&child_metadata::ChildCreation>,
) -> Result<NatsSession, ToolInvokeError> {
    let requested_id = normalize_session_id(args.session_id.clone());
    let tool_call_id = args.tool_call_id.as_deref();
    let session_id = match context.checkpoint.as_ref() {
        Some(saved) => validate_checkpoint(saved, toolset, parent, &context.call_id)?,
        None => match requested_id {
            Some(id) => id,
            None => {
                // A patch-bearing create folds its metadata into the
                // reservation initializer, so the reservation's atomic
                // `creation_invocation` stamp is the creation guard.
                let config = toolset
                    .session_config_with_creation(None, Some(parent), tool_call_id, creation)
                    .await?;
                crate::utils::session_name::reserve_invocation_session_id(
                    &toolset.session_metadata,
                    &config.initializer,
                    &context.call_id,
                    context
                        .run_context
                        .as_ref()
                        .expect("validated inherited context")
                        .started_at_ms,
                )
                .await
                .map_err(recoverable)?
            }
        },
    };
    let checkpoint = checkpoint_value(toolset, &session_id, parent, &context.call_id);
    let session_id = if let Some(store) = context.checkpoint_store.as_ref() {
        let stored = store.checkpoint(checkpoint).await.map_err(|error| {
            ToolInvokeError::Fatal(format!("persist sub-agent dispatch checkpoint: {error:#}"))
        })?;
        validate_checkpoint(&stored, toolset, parent, &context.call_id)?
    } else {
        session_id
    };
    toolset
        .create_session_with_claim(Some(session_id), Some(parent), tool_call_id, creation)
        .await
        .map(|(session, _)| session)
}

fn checkpoint_value(
    toolset: &SubagentToolset,
    session_id: &str,
    parent: &str,
    prompt_id: &str,
) -> Value {
    let storage = harnx_core::session_identity::session_key(Some(&toolset.agent), session_id);
    json!({"session_id": session_id, "storage_key": storage, "cluster": toolset.route.cluster(), "prompt_id": prompt_id, "parent_storage_key": parent})
}

fn validate_checkpoint(
    saved: &Value,
    toolset: &SubagentToolset,
    parent: &str,
    prompt_id: &str,
) -> Result<String, ToolInvokeError> {
    // The dispatch checkpoint pins the same core scope (session id, storage
    // key, cluster) plus its prompt and parent identity (review F03); the
    // shared core owns the comparison.
    child_metadata::saved_checkpoint_scope(
        toolset,
        saved,
        &[("prompt_id", prompt_id), ("parent_storage_key", parent)],
    )
    .map_err(|mismatch| {
        ToolInvokeError::Fatal(
            match mismatch {
                child_metadata::ScopeMismatch::MissingId => {
                    "invalid sub-agent dispatch checkpoint session_id"
                }
                child_metadata::ScopeMismatch::Scope => {
                    "sub-agent dispatch checkpoint scope mismatch"
                }
            }
            .into(),
        )
    })
}

async fn record_child(
    toolset: &SubagentToolset,
    context: &ToolInvocationContext,
    session_id: &str,
) -> Result<(), ToolInvokeError> {
    let partial = json!({"session_id": session_id, "prompt_id": context.call_id, "dispatch": null, "sub_agent": {"agent": toolset.agent, "session_id": session_id}});
    // Without this write, losing the append/activation reply could also lose
    // the recoverable handle. Fail before executable append when it fails.
    context
        .record_partial_result(partial)
        .await
        .map_err(|error| {
            ToolInvokeError::Fatal(format!(
                "persist sub-agent dispatch handle before append: {error:#}"
            ))
        })
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ToolInvokeError> {
    if cancel.is_cancelled() {
        Err(ToolInvokeError::Fatal("sub-agent tool call aborted".into()))
    } else {
        Ok(())
    }
}

fn recoverable(error: anyhow::Error) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!("sub-agent dispatch: {error:#}"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod live_tests;
