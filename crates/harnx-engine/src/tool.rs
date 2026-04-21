//! Tool-call loop — `eval_tool_calls` orchestrates a batch of tool
//! dispatches with pre/post hooks, user confirmation, UI emission,
//! and per-call abort handling. The loop is provider-agnostic: it
//! iterates `Vec<Arc<dyn ToolProvider>>` (harnx-core trait). All
//! harnx-specific concerns — UI rendering, hook dispatch execution,
//! inquire prompts — are injected via callbacks on `ToolEvalContext`,
//! constructed on the harnx side by `build_tool_eval_context`.

use anyhow::{anyhow, bail, Result};
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::hooks::{HookEvent, HookOutcome, HookResult, HookResultControl};
use harnx_core::tool::{
    SwitchAgentData, ToolCall, ToolError, ToolProvider, ToolResult, TRIGGER_AGENT_TOOL_NAME,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::runtime::Handle;

/// Callback invoked with a `&ToolCall` and the parsed arguments JSON.
/// Used for both "tool is about to dispatch" and "tool returned a
/// result" UI emission hooks on `ToolEvalContext`.
pub type ToolCallEmitFn = dyn Fn(&ToolCall, &Value) + Send + Sync;

/// Callback invoked when a PreToolUse hook returns `Ask { reason }`.
/// Receives the tool name, parsed arguments, and optional reason.
/// Returns `true` if the user allows the tool; `false` otherwise.
pub type ConfirmToolUseFn = dyn Fn(&str, &Value, Option<&str>) -> bool + Send + Sync;

/// Callback that dispatches a hook event and resolves to an outcome.
/// Takes the `HookEvent` by value (events are constructed fresh at
/// each site) and returns a boxed future. Captured state (hook
/// entries, session id, cwd) is owned by the closure so the returned
/// future is `'static` and can cross `tokio::select!` and
/// `block_in_place` boundaries.
pub type DispatchHookFn =
    dyn Fn(HookEvent) -> Pin<Box<dyn Future<Output = HookOutcome> + Send>> + Send + Sync;

/// Narrow runtime context for one batch of tool calls. Constructed on
/// the harnx side (see `harnx::tool::build_tool_eval_context`) from a
/// `GlobalConfig` snapshot plus harnx's default callback implementations.
/// The loop reads nothing from the context beyond these fields.
pub struct ToolEvalContext {
    pub providers: Vec<Arc<dyn ToolProvider>>,
    pub session_name: Option<String>,
    pub allowed_tool_names: HashSet<String>,
    /// Called when a tool is about to be dispatched. Receives the tool
    /// call and the parsed arguments JSON. Harnx's default formats
    /// input as YAML and emits a `ToolCall` UiOutputEvent, falling
    /// back to stdout if no UI sink is installed.
    pub emit_tool_call_fn: Arc<ToolCallEmitFn>,
    /// Called when a tool call returns a result. Receives the tool
    /// call and the raw result JSON. Harnx's default extracts
    /// user-display text (or YAML-pretty-prints the JSON), truncates
    /// to terminal dimensions, dims the text, and emits a
    /// `ToolResultText` UiOutputEvent with stdout fallback.
    pub emit_tool_result_fn: Arc<ToolCallEmitFn>,
    /// Called when a PreToolUse hook returns `Ask { reason }` and the
    /// user needs to confirm before the tool runs. Returns `true` if
    /// the user allows the tool; `false` otherwise. Harnx's default
    /// uses an `inquire`-based terminal prompt.
    pub confirm_tool_use_fn: Arc<ConfirmToolUseFn>,
    /// Called to dispatch a hook event (PreToolUse, PostToolUse,
    /// PostToolUseFailure). Harnx's default captures `hooks.entries`,
    /// `session_id` (currently always `"cmd"`), and the process cwd
    /// at context-construction time and forwards to
    /// `hooks::dispatch::dispatch_hooks`.
    pub dispatch_hook_fn: Arc<DispatchHookFn>,
}

pub fn eval_tool_calls(
    ctx: &ToolEvalContext,
    mut calls: Vec<ToolCall>,
    abort_signal: &AbortSignal,
) -> Result<Vec<ToolResult>> {
    let mut output = vec![];
    if calls.is_empty() {
        return Ok(output);
    }
    calls = ToolCall::dedup(calls);
    if calls.is_empty() {
        bail!("The request was aborted because an infinite loop of function calls was detected.")
    }

    let mut is_all_null = true;
    for call in calls {
        let tool_input = call.arguments.clone();
        let tool_use_id = call.id.clone().unwrap_or_default();

        let pre_event = HookEvent::PreToolUse {
            tool_name: call.name.clone(),
            tool_input: tool_input.clone(),
            tool_use_id: tool_use_id.clone(),
        };
        let pre_outcome = tokio::task::block_in_place(|| {
            Handle::current().block_on(async {
                tokio::select! {
                    outcome = (ctx.dispatch_hook_fn)(pre_event) => outcome,
                    _ = wait_abort_signal(abort_signal) => HookOutcome {
                        control: HookResultControl::Block {
                            reason: "cancelled by user".to_string(),
                        },
                        result: HookResult::default(),
                    },
                }
            })
        });
        if abort_signal.aborted() {
            bail!("interrupted during pre-tool hook");
        }
        if let HookResultControl::Block { reason } = pre_outcome.control {
            let blocked_result = json!({"error": reason, "blocked_by_hook": true});
            output.push(ToolResult::new(call, blocked_result));
            is_all_null = false;
            continue;
        }
        if let HookResultControl::Ask { reason } = pre_outcome.control {
            if !(ctx.confirm_tool_use_fn)(&call.name, &call.arguments, reason.as_deref()) {
                let deny_reason = reason.unwrap_or_else(|| "Denied by user".to_string());
                let blocked_result = json!({"error": deny_reason, "blocked_by_hook": true});
                output.push(ToolResult::new(call, blocked_result));
                is_all_null = false;
                continue;
            }
        }

        // Short-circuit remaining tool calls if a cancel already fired.
        if abort_signal.aborted() {
            bail!("tool execution aborted by user");
        }

        let eval_result = eval_tool_call_mcp(&call, ctx, abort_signal);
        match eval_result {
            Ok(mut result) => {
                let post_event = HookEvent::PostToolUse {
                    tool_name: call.name.clone(),
                    tool_input: tool_input.clone(),
                    tool_response: result.clone(),
                    tool_use_id: tool_use_id.clone(),
                };
                let _ = tokio::task::block_in_place(|| {
                    Handle::current().block_on((ctx.dispatch_hook_fn)(post_event))
                });

                // Emit tool result to TUI or terminal
                (ctx.emit_tool_result_fn)(&call, &result);

                if result.is_null() {
                    result = json!("DONE");
                } else {
                    is_all_null = false;
                }
                let mut result_obj = ToolResult::new(call, result);
                if let Some(obj) = result_obj.output.as_object() {
                    if obj.get("action").and_then(|v| v.as_str()) == Some("switch_agent") {
                        if let (Some(agent), Some(prompt)) = (
                            obj.get("agent").and_then(|v| v.as_str()),
                            obj.get("prompt").and_then(|v| v.as_str()),
                        ) {
                            result_obj.switch_agent = Some(SwitchAgentData {
                                agent: agent.to_string(),
                                prompt: prompt.to_string(),
                                session_id: obj
                                    .get("session_id")
                                    .and_then(|v| v.as_str())
                                    .map(ToString::to_string),
                            });
                        }
                    }
                }
                output.push(result_obj);
            }
            Err(ToolError::Recoverable(err)) => {
                let error_display = format!("{err:#}");
                let fail_event = HookEvent::PostToolUseFailure {
                    tool_name: call.name.clone(),
                    tool_input: tool_input.clone(),
                    tool_use_id: tool_use_id.clone(),
                    error: error_display.clone(),
                };
                let _ = tokio::task::block_in_place(|| {
                    Handle::current().block_on((ctx.dispatch_hook_fn)(fail_event))
                });

                is_all_null = false;
                let error_result = json!({
                    "is_error": true,
                    "error": error_display,
                });
                output.push(ToolResult::new(call, error_result));
            }
            Err(ToolError::Fatal(err)) => return Err(err),
        }
    }
    if is_all_null {
        output = vec![];
    }
    Ok(output)
}

fn eval_tool_call_mcp(
    call: &ToolCall,
    ctx: &ToolEvalContext,
    abort_signal: &AbortSignal,
) -> Result<Value, ToolError> {
    let json_data = if call.arguments.is_null() {
        Value::Null
    } else if call.arguments.is_object() {
        call.arguments.clone()
    } else if let Some(arguments) = call.arguments.as_str() {
        serde_json::from_str(arguments).map_err(|_| {
            ToolError::Recoverable(anyhow!(
                "The call '{}' has invalid arguments: {arguments}",
                call.name
            ))
        })?
    } else {
        return Err(ToolError::Recoverable(anyhow!(
            "The call '{}' has invalid arguments: {}",
            call.name,
            call.arguments
        )));
    };

    // Emit tool call info to TUI or terminal
    (ctx.emit_tool_call_fn)(call, &json_data);

    if call.name == TRIGGER_AGENT_TOOL_NAME {
        let agent = json_data["agent"].as_str().ok_or_else(|| {
            ToolError::Recoverable(anyhow!("Missing 'agent' argument for trigger_agent"))
        })?;
        let prompt = json_data["prompt"].as_str().ok_or_else(|| {
            ToolError::Recoverable(anyhow!("Missing 'prompt' argument for trigger_agent"))
        })?;

        return Ok(json!({
            "status": "success",
            "message": format!("Transferring session to agent '{}'...", agent),
            "action": "switch_agent",
            "agent": agent,
            "prompt": prompt
        }));
    }

    let allowed_tool_names = &ctx.allowed_tool_names;

    if call.name.ends_with("_session_handoff") {
        if !allowed_tool_names.contains(&call.name) {
            return Err(ToolError::Recoverable(anyhow!(
                "No tool provider configured for '{}'",
                call.name
            )));
        }
        let agent = call.name.trim_end_matches("_session_handoff");
        let prompt = json_data["prompt"].as_str().ok_or_else(|| {
            ToolError::Recoverable(anyhow!("Missing 'prompt' argument for session handoff"))
        })?;
        let session_id = json_data["session_id"]
            .as_str()
            .map(ToString::to_string)
            .or_else(|| ctx.session_name.clone());

        return Ok(json!({
            "content": [{
                "type": "text",
                "text": format!("Handing off to {}…", agent),
                "annotations": { "audience": ["user"] }
            }],
            "action": "switch_agent",
            "agent": agent,
            "prompt": prompt,
            "session_id": session_id,
        }));
    }

    for provider in &ctx.providers {
        if !provider.has_tool(&call.name) {
            continue;
        }
        let tool_name = call.name.clone();
        let args = json_data.clone();
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(provider.call_tool(
                &tool_name,
                args,
                abort_signal,
            ))
        })?;
        return Ok(result);
    }

    Err(ToolError::Recoverable(anyhow!(
        "No tool provider configured for '{}'",
        call.name
    )))
}
