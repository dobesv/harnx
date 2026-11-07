//! Send admission and delivery only. Observation and discovery are separate.
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
    let budget = wait.as_ref().map_or(
        super::super::session_read::DEFAULT_MAX_OUTPUT_BYTES as usize,
        |wait| wait.read.max_output_bytes,
    );
    let requested_id = normalize_session_id(args.session_id.clone());
    // New IDs are six ASCII bytes. Include the diagnostic invocation identity
    // in preflight; Task1's handle-only reservation intentionally omits it.
    ensure_receipt_fits(
        requested_id.as_deref().unwrap_or("000000"),
        &context.call_id,
        budget,
    )?;
    check_cancel(&cancel)?;
    let session = checkpointed_session(toolset, &args, &inherited.parent_key, &context).await?;
    ensure_receipt_fits(session.session_id(), &context.call_id, budget)?;
    let session = session
        .with_inherited_admission(
            inherited.parent,
            context.call_id.clone(),
            inherited.admitted_at,
            InvocationEdgeKind::Delegation,
            args.timeout_secs,
        )
        .with_execution_parent(inherited.parent_key, context.call_id.clone());
    // Replayed delivery never re-fetches attachments or replaces original
    // content, even if the referenced resource disappeared after admission.
    let content = match toolset
        .session_metadata
        .admission(session.storage_key(), &context.call_id)
        .await
        .map_err(recoverable)?
    {
        Some(saved) => saved.prompt_content.ok_or_else(|| {
            ToolInvokeError::Fatal("dispatch admission has no original content".into())
        })?,
        None => {
            toolset
                .compose_prompt_content(args.message, args.attachments.unwrap_or_default())
                .await?
        }
    };
    check_cancel(&cancel)?;
    record_child(toolset, &context, session.session_id()).await?;
    // Parent observer cancellation is deliberately not installed on the child.
    // Explicit parent cancellation and hard deadlines retain worker authority.
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
    let lineage = context.run_context.as_ref().ok_or_else(|| {
        ToolInvokeError::Fatal("sub-agent execution requires inherited run context".into())
    })?;
    let parent = serde_json::from_value(lineage.snapshot.clone()).map_err(|error| {
        ToolInvokeError::Fatal(format!("invalid inherited run context: {error}"))
    })?;
    let admitted_at = chrono::DateTime::from_timestamp_millis(
        i64::try_from(lineage.started_at_ms)
            .map_err(|error| ToolInvokeError::Fatal(error.to_string()))?,
    )
    .ok_or_else(|| ToolInvokeError::Fatal("invalid original invocation time".into()))?;
    Ok(InheritedDispatch {
        parent_key,
        parent,
        admitted_at,
    })
}

fn ensure_receipt_fits(
    session_id: &str,
    prompt_id: &str,
    budget: usize,
) -> Result<(), ToolInvokeError> {
    let receipt = SessionPromptResult {
        session_id: session_id.into(),
        prompt_id: prompt_id.into(),
        invocation_id: Some(prompt_id.into()),
        dispatch: Some(DispatchConfirmation::Unconfirmed),
        status: SessionPromptStatus::Cancelled,
        terminal: false,
        wait_expired: false,
        elapsed_ms: u64::MAX,
        output: Value::Null,
    };
    let size = serde_json::to_vec(&receipt)
        .map_err(|error| ToolInvokeError::Fatal(error.to_string()))?
        .len();
    if size > budget {
        return Err(ToolInvokeError::Recoverable(format!(
            "dispatch result metadata ({size} bytes) exceeds max_output_bytes budget {budget}"
        )));
    }
    Ok(())
}

async fn checkpointed_session(
    toolset: &SubagentToolset,
    args: &super::SendArgs,
    parent: &str,
    context: &ToolInvocationContext,
) -> Result<NatsSession, ToolInvokeError> {
    let requested_id = normalize_session_id(args.session_id.clone());
    let tool_call_id = args.tool_call_id.as_deref();
    let session_id = match context.checkpoint.as_ref() {
        Some(saved) => validate_checkpoint(saved, toolset, parent, &context.call_id)?,
        None => match requested_id {
            Some(id) => id,
            None => {
                let config = toolset
                    .session_config(None, Some(parent), tool_call_id)
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
        .create_session(Some(session_id), Some(parent), tool_call_id)
        .await
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
    let id = saved["session_id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            ToolInvokeError::Fatal("invalid sub-agent dispatch checkpoint session_id".into())
        })?;
    let expected = checkpoint_value(toolset, id, parent, prompt_id);
    if !expected
        .as_object()
        .expect("checkpoint object")
        .iter()
        .all(|(key, value)| saved.get(key) == Some(value))
    {
        return Err(ToolInvokeError::Fatal(
            "sub-agent dispatch checkpoint scope mismatch".into(),
        ));
    }
    Ok(id.into())
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
