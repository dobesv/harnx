use crate::{
    acp::{AcpManager, NestedAcpEvent},
    config::GlobalConfig,
    hooks::{HookEvent, HookOutcome, HookResult, HookResultControl},
    mcp::McpManager,
    mcp_safety::{truncate_output, TruncateOpts},
    tui::render_helpers::event_fallback_text,
    ui_output::{
        emit_ui_output_event, has_ui_output_sink, pretty_yaml_block, UiOutputEvent,
        UiOutputEventKind,
    },
    utils::*,
};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::future::Future;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;

pub use harnx_core::tool::{
    extract_user_display_text, trigger_agent_tool_declaration, JsonSchema, SwitchAgentData,
    ToolCall, ToolDeclaration, ToolError, ToolResult, Tools, TRIGGER_AGENT_TOOL_NAME,
};

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

/// Narrow view of `GlobalConfig` used by the tool-call loop. Callers
/// construct this once per batch via `ToolEvalContext::from_config`
/// and pass it through. This decouples the loop from `GlobalConfig`
/// as prep for moving the loop into `harnx-engine` — at that point
/// the loop can be moved without any Config dep.
pub struct ToolEvalContext {
    pub acp_manager: Option<Arc<AcpManager>>,
    pub mcp_manager: Option<Arc<McpManager>>,
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

impl ToolEvalContext {
    pub fn from_config(config: &GlobalConfig) -> Self {
        let guard = config.read();
        let use_tools = guard.extract_agent().use_tools().map(|v| v.join(","));
        let allowed_tool_names: HashSet<String> = guard
            .tool_declarations_for_use_tools(use_tools.as_deref())
            .into_iter()
            .map(|decl| decl.name)
            .collect();
        let hooks = guard.resolved_hooks();

        // Capture owned state for the dispatch callback so the
        // returned future is `'static` and `Send`.
        let hooks_entries = hooks.entries.clone();
        let session_id = "cmd".to_string();
        let cwd = std::env::current_dir().unwrap_or_default();
        let dispatch_hook_fn: Arc<DispatchHookFn> = Arc::new(move |event: HookEvent| {
            let hooks_entries = hooks_entries.clone();
            let session_id = session_id.clone();
            let cwd = cwd.clone();
            Box::pin(async move {
                crate::hooks::dispatch::dispatch_hooks(&event, &hooks_entries, &session_id, &cwd)
                    .await
            })
        });

        Self {
            acp_manager: guard.acp_manager.clone(),
            mcp_manager: guard.mcp_manager.clone(),
            session_name: guard
                .session
                .as_ref()
                .map(|session| session.name().to_string()),
            allowed_tool_names,
            emit_tool_call_fn: Arc::new(default_emit_tool_call),
            emit_tool_result_fn: Arc::new(default_emit_tool_result),
            confirm_tool_use_fn: Arc::new(default_confirm_tool_use),
            dispatch_hook_fn,
        }
    }
}

fn default_emit_tool_call(call: &ToolCall, json_data: &Value) {
    let event = UiOutputEvent {
        kind: UiOutputEventKind::ToolCall {
            tool_name: call.name.clone(),
            input_yaml: match json_data {
                Value::Null => None,
                _ => Some(pretty_yaml_block(json_data)),
            },
            raw: None,
        },
        source: None,
    };
    let text = event_fallback_text(&event.kind, event.source.as_ref());
    if !emit_ui_output_event(event) && *IS_STDOUT_TERMINAL {
        print!("{text}");
    }
}

fn default_emit_tool_result(call: &ToolCall, result: &Value) {
    let mut opts = TruncateOpts::default();
    let marker = " [...] ";
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        opts.head_lines = 5.max((rows / 2) as usize);
        opts.tail_lines = 0;
        // "<= " prefix is 3 chars, marker is 7 chars; total overhead = 10
        // line_head_bytes + marker.len() + prefix.len() must fit in cols
        opts.line_head_bytes = (cols as usize).saturating_sub(3 + marker.len());
        opts.line_tail_bytes = 0;
        opts.marker = Some(marker.to_string());
    }
    let output_str = extract_user_display_text(result).unwrap_or_else(|| match result {
        Value::String(s) => s.clone(),
        _ => pretty_yaml_block(result),
    });
    let truncated = truncate_output(&output_str, &opts);
    let text = format!("{}\n", dimmed_text(&truncated));
    let event = UiOutputEvent {
        kind: UiOutputEventKind::ToolResultText { text: text.clone() },
        source: None,
    };
    if !emit_ui_output_event(event) && *IS_STDOUT_TERMINAL {
        print!("{text}");
    }
    // `call` parameter is unused by the default implementation but
    // kept in the signature so custom callbacks can route per-call.
    let _ = call;
}

fn default_confirm_tool_use(tool_name: &str, arguments: &Value, reason: Option<&str>) -> bool {
    crate::hooks::prompt::confirm_tool_use(tool_name, arguments, reason)
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

    let acp_manager = ctx.acp_manager.clone();
    if let Some(manager) = acp_manager {
        if manager.find_client_for_tool(&call.name).is_some() {
            let tool_name = call.name.clone();
            // call_tool internally races session_prompt against
            // Ctrl+C and cancels the ACP session (including
            // auto-created sessions) when the user aborts.
            //
            // Subscribe to ACP chunk notifications so that tool calls,
            // agent thoughts, plan updates, and text chunks from the
            // sub-agent (and any nested sub-sub-agents) are printed to
            // stdout in real-time instead of being silently swallowed.
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let has_ui_output = has_ui_output_sink();
                    let is_terminal = *IS_STDOUT_TERMINAL && !has_ui_output;

                    // Give the parent tool-call event a chance to be
                    // consumed by the UI before nested ACP output from the
                    // delegated session starts arriving. This keeps the
                    // visible transcript ordering causal: the handoff tool
                    // row should appear before any child output it triggers.
                    tokio::task::yield_now().await;

                    let (chunk_rx, subscription_id) = manager.subscribe_chunks().await;

                    // Spawn a spinner only in non-TUI terminal mode.
                    let spinner = if is_terminal {
                        Some(spawn_spinner(&format!("  {} working…", tool_name)))
                    } else {
                        None
                    };

                    // Forward chunks either to TUI output sink or stdout.
                    let spinner_clone = spinner.clone();
                    let spinner_msg = format!("  {} working…", tool_name);
                    let forward_handle = tokio::spawn(forward_acp_chunks(
                        chunk_rx,
                        spinner_clone,
                        spinner_msg,
                        is_terminal,
                    ));

                    // Race the sub-agent call against our abort_signal
                    // so Ctrl-C interrupts nested ACP delegations the
                    // same way it interrupts MCP tools.
                    let call_result = tokio::select! {
                        result = manager.call_tool(&tool_name, json_data) => result,
                        _ = wait_abort_signal(abort_signal) => {
                            Err(anyhow!("ACP tool call aborted by user"))
                        }
                    };

                    // Tear down: unsubscribe first (closes the
                    // channel), then await the forward task.
                    manager.unsubscribe_chunks(subscription_id).await;
                    forward_handle.abort();
                    let _ = forward_handle.await;

                    if let Some(s) = spinner {
                        s.stop();
                    }

                    call_result
                })
            })
            .map_err(|err| {
                // Only user-initiated aborts (Ctrl+C) are fatal and should
                // stop the entire tool batch.  Other failures (timeouts,
                // connection errors, bad arguments) are recoverable so the
                // LLM receives the error message and can retry.
                if err.to_string().contains("aborted by user") {
                    ToolError::Fatal(err)
                } else {
                    ToolError::Recoverable(err)
                }
            })?;

            return Ok(result);
        }
    }

    let mcp_manager = ctx.mcp_manager.clone();
    let manager = match mcp_manager {
        Some(m) => m,
        None => {
            return Err(ToolError::Recoverable(anyhow!(
                "No tool provider configured for '{}'",
                call.name
            )))
        }
    };

    let tool_name = call.name.clone();
    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            tokio::select! {
                result = manager.call_tool(&tool_name, json_data) => result.map_err(ToolError::Recoverable),
                _ = wait_abort_signal(abort_signal) => {
                    Err(ToolError::Fatal(anyhow!("MCP tool call aborted by user")))
                }
            }
        })
    })?;

    Ok(result)
}

/// Forwards ACP chunk notifications to stdout with spinner management.
///
/// Each incoming chunk temporarily clears the spinner, prints the chunk,
/// then restores the spinner.  This gives the user live visibility into
/// sub-agent (and sub-sub-agent) tool calls, thoughts, and plan updates.
async fn forward_acp_chunks(
    mut chunk_rx: UnboundedReceiver<NestedAcpEvent>,
    spinner: Option<Spinner>,
    spinner_msg: String,
    allow_fallback_print: bool,
) {
    while let Some(chunk) = chunk_rx.recv().await {
        // Pause the spinner (clear display but keep task alive) before
        // printing so output is clean, then resume it afterwards.
        if let Some(ref s) = spinner {
            s.pause();
        }
        match chunk {
            NestedAcpEvent::Ui(event) => {
                let fallback = event_fallback_text(&event.kind, event.source.as_ref());
                if !emit_ui_output_event(event) && allow_fallback_print {
                    print!("{fallback}");
                    let _ = std::io::stdout().flush();
                }
            }
            NestedAcpEvent::Text(text) => {
                let event = UiOutputEvent {
                    kind: UiOutputEventKind::TranscriptText { text: text.clone() },
                    source: None,
                };
                if !emit_ui_output_event(event) && allow_fallback_print {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
            }
        }
        // Re-enable the spinner after printing.
        if let Some(ref s) = spinner {
            let _ = s.set_message(spinner_msg.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ui_output::{clear_ui_output_sender, install_ui_output_sender};
    use indexmap::IndexMap;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn test_mcp_tool_invocation_terminal_fallback_multiline_yaml() {
        let rendered = event_fallback_text(
            &UiOutputEventKind::ToolCall {
                tool_name: "argus_session_prompt".to_string(),
                input_yaml: Some(pretty_yaml_block(&json!({
                    "message": "Goal — Improve display\nAcceptance criteria — Wrap nicely",
                    "session_id": "session-1"
                }))),
                raw: None,
            },
            None,
        );

        assert!(rendered.contains("argus_session_prompt"));
        assert!(rendered.contains("message:"));
        assert!(rendered.contains("Acceptance criteria"));
        assert!(rendered.contains("session_id:"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_eval_tool_calls_error_handling() {
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(RwLock::new(Config::default()));
        let call = ToolCall::new(
            "unknown_tool".to_string(),
            json!({}),
            Some("1".to_string()),
            None,
        );
        let calls = vec![call];

        let abort_signal = create_abort_signal();
        let result =
            eval_tool_calls(&ToolEvalContext::from_config(&config), calls, &abort_signal).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].call.name, "unknown_tool");
        assert!(result[0].output.is_object());
        assert_eq!(result[0].output["is_error"], true);
        assert!(result[0].output["error"]
            .as_str()
            .unwrap()
            .contains("No tool provider configured"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_eval_tool_calls_partial_error_handling() {
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(RwLock::new(Config::default()));
        // ACP handoff tools should be handled via the ACP manager and unknown tools still error.
        let call1 = ToolCall::new(
            TRIGGER_AGENT_TOOL_NAME.to_string(),
            json!({"agent": "test", "prompt": "test"}),
            Some("1".to_string()),
            None,
        );
        let call2 = ToolCall::new(
            "unknown_tool".to_string(),
            json!({}),
            Some("2".to_string()),
            None,
        );
        let calls = vec![call1, call2];

        let abort_signal = create_abort_signal();
        let result =
            eval_tool_calls(&ToolEvalContext::from_config(&config), calls, &abort_signal).unwrap();
        assert_eq!(result.len(), 2);

        assert_eq!(result[0].call.name, TRIGGER_AGENT_TOOL_NAME);
        assert_eq!(result[0].output["action"], "switch_agent");

        assert_eq!(result[1].call.name, "unknown_tool");
        assert_eq!(result[1].output["is_error"], true);
    }

    #[test]
    fn test_tool_result_event_fallback_text_matches_terminal_output() {
        let text = "\u{1b}[2mtool output\u{1b}[0m\n".to_string();
        let event = UiOutputEventKind::ToolResultText { text: text.clone() };

        assert_eq!(event_fallback_text(&event, None), text);
    }

    #[tokio::test]
    async fn forward_acp_chunks_preserves_structured_nested_events() {
        let (ui_tx, mut ui_rx) = unbounded_channel();
        install_ui_output_sender(ui_tx);

        let (tx, rx) = unbounded_channel();
        let forward = tokio::spawn(forward_acp_chunks(rx, None, "working".to_string(), false));

        tx.send(NestedAcpEvent::Ui(UiOutputEvent {
            kind: UiOutputEventKind::ToolCall {
                tool_name: "argus_session_prompt".to_string(),
                input_yaml: Some("message: hi".to_string()),
                raw: None,
            },
            source: Some(crate::ui_output::UiOutputSource {
                agent: "argus".to_string(),
                session_id: Some("sess-1".to_string()),
            }),
        }))
        .unwrap();
        drop(tx);

        forward.await.unwrap();

        let event = ui_rx.recv().await.expect("forwarded UI event");
        match event.kind {
            UiOutputEventKind::ToolCall {
                tool_name,
                input_yaml,
                ..
            } => {
                assert_eq!(tool_name, "argus_session_prompt");
                assert_eq!(input_yaml.as_deref(), Some("message: hi"));
            }
            other => panic!("unexpected event kind: {other:?}"),
        }

        clear_ui_output_sender();
    }

    #[test]
    fn test_flatten_any_of_nullable_array() {
        // Simulates Option<Vec<String>> schema: anyOf: [{type: "array", items: {type: "string"}}, {type: "null"}]
        let schema = JsonSchema {
            type_value: Some("object".to_string()),
            properties: Some(IndexMap::from([(
                "tags".to_string(),
                JsonSchema {
                    description: Some("Optional tags".to_string()),
                    any_of: Some(vec![
                        JsonSchema {
                            type_value: Some("array".to_string()),
                            items: Some(Box::new(JsonSchema {
                                type_value: Some("string".to_string()),
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                        JsonSchema {
                            type_value: Some("null".to_string()),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };

        let flattened = schema.flatten_any_of();
        let props = flattened.properties.unwrap();
        let tags = props.get("tags").unwrap();

        // anyOf should be resolved: the property should now be a plain array
        assert!(tags.any_of.is_none());
        assert_eq!(tags.type_value.as_deref(), Some("array"));
        assert_eq!(tags.description.as_deref(), Some("Optional tags"));
        assert_eq!(
            tags.items.as_ref().and_then(|i| i.type_value.as_deref()),
            Some("string")
        );
    }

    #[test]
    fn test_flatten_any_of_no_change_for_plain_schema() {
        let schema = JsonSchema {
            type_value: Some("string".to_string()),
            description: Some("A name".to_string()),
            ..Default::default()
        };
        let flattened = schema.flatten_any_of();
        assert_eq!(flattened.type_value.as_deref(), Some("string"));
        assert_eq!(flattened.description.as_deref(), Some("A name"));
    }
}
