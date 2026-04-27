use crate::{
    config::{GlobalConfig, Input},
    utils::*,
};
use anyhow::Result;
use harnx_hooks::HookEvent;
use harnx_mcp::safety::{truncate_output, TruncateOpts};

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use harnx_core::tool::ToolProvider;
pub use harnx_core::tool::{
    extract_user_display_text, render_tool_call_template, render_tool_result_template, JsonSchema,
    SwitchAgentData, ToolCall, ToolDeclaration, ToolResult, Tools,
};
pub use harnx_engine::tool::{
    eval_tool_calls, ConfirmToolUseFn, DispatchHookFn, ToolCallEmitFn, ToolEvalContext,
};

/// Persist a tool round and execute its calls.  Writes the
/// `ToolCalls` session-log entry BEFORE running tools (so the
/// transcript captures the request even on crash/interrupt), runs
/// `eval_tool_calls`, then writes the matching `ToolResults` entry.
///
/// On eval failure, synthesizes one error-output `ToolResult` per
/// call, writes those to keep the log well-formed, and returns the
/// original error.  Skips both writes entirely when `dry_run` is set.
pub async fn execute_tool_round(
    config: &GlobalConfig,
    input: &Input,
    output: &str,
    thought: Option<&str>,
    tool_calls: Vec<ToolCall>,
    abort_signal: &AbortSignal,
) -> Result<Vec<ToolResult>> {
    let dry_run = config.read().dry_run;
    if !dry_run {
        config
            .write()
            .save_session_tool_calls(input, output, thought, &tool_calls)?;
    }
    let agent_use_tools = input.agent().use_tools().map(|v| v.join(","));
    let eval_ctx = build_tool_eval_context(config, agent_use_tools.as_deref());
    let results = match eval_tool_calls(&eval_ctx, tool_calls.clone(), abort_signal).await {
        Ok(results) => results,
        Err(err) => {
            let fallback: Vec<ToolResult> = tool_calls
                .into_iter()
                .map(|call| {
                    ToolResult::new(
                        call,
                        serde_json::json!({
                            "error": format!("tool execution failed: {err:#}")
                        }),
                    )
                })
                .collect();
            if !dry_run {
                let _ = config.write().save_session_tool_results(&fallback);
            }
            return Err(err);
        }
    };
    if !dry_run {
        config.write().save_session_tool_results(&results)?;
    }
    Ok(results)
}

/// Build a `ToolEvalContext` from the harnx `GlobalConfig`. Replaces the
/// old inherent `ToolEvalContext::from_config` method — the struct lives
/// in `harnx-engine::tool` now (orphan rules forbid adding inherent
/// methods on a cross-crate type). Snapshots Config fields, constructs
/// the provider list (ACP first, MCP second), builds the dispatch hook
/// closure over captured `hooks.entries`, `session_id`, and `cwd`, and
/// wires in harnx-side default UI/prompt callbacks.
///
/// `agent_use_tools` is the active agent's `use_tools` whitelist. The
/// CLI/TUI flow stores the agent on the Config (via `use_agent`), so
/// `Config::extract_agent()` would yield the right value, but the ACP
/// server holds the agent only on the per-prompt `Input` (because each
/// `prompt` call may target a different agent on the same Config).
/// Passing the use_tools list explicitly keeps both paths correct.
pub fn build_tool_eval_context(
    config: &GlobalConfig,
    agent_use_tools: Option<&str>,
) -> ToolEvalContext {
    let guard = config.read();
    let decl_map: Arc<HashMap<String, ToolDeclaration>> = Arc::new(
        guard
            .tool_declarations_for_use_tools(agent_use_tools)
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect(),
    );
    let allowed_tool_names: HashSet<String> = decl_map.keys().cloned().collect();
    let hooks = guard.resolved_hooks();
    let acp_manager = guard.acp_manager.clone();
    let mcp_manager = guard.mcp_manager.clone();
    let session_name = guard
        .session
        .as_ref()
        .map(|session| session.name().to_string());
    drop(guard);

    // Build the provider list in ACP-first order so ACP sub-agent
    // handoffs take priority over any namespaced MCP tool with the
    // same name.
    let mut providers: Vec<Arc<dyn ToolProvider>> = Vec::new();
    if let Some(acp) = acp_manager {
        providers.push(acp as Arc<dyn ToolProvider>);
    }
    if let Some(mcp) = mcp_manager {
        providers.push(mcp as Arc<dyn ToolProvider>);
    }

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
            harnx_hooks::dispatch::dispatch_hooks(&event, &hooks_entries, &session_id, &cwd).await
        })
    });

    let decl_map_clone = Arc::clone(&decl_map);
    let emit_tool_call_fn: Arc<ToolCallEmitFn> =
        Arc::new(move |call: &ToolCall, json_data: &Value| {
            emit_tool_call_with_template(call, json_data, &decl_map_clone);
        });

    let decl_map_clone2 = Arc::clone(&decl_map);
    let emit_tool_result_fn: Arc<ToolCallEmitFn> =
        Arc::new(move |call: &ToolCall, result: &Value| {
            emit_tool_result_with_template(call, result, &decl_map_clone2);
        });

    let confirm_tool_use_fn: Arc<ConfirmToolUseFn> = Arc::new(default_confirm_tool_use);

    ToolEvalContext {
        providers,
        session_name,
        allowed_tool_names,
        emit_tool_call_fn,
        emit_tool_result_fn,
        confirm_tool_use_fn,
        dispatch_hook_fn,
    }
}

/// Look up and render the call template for a tool, returning rendered string or None.
/// Logs warning to stderr on render error.
fn render_call(
    call: &ToolCall,
    json_data: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    let tmpl = decl_map.get(&call.name)?.call_template.as_ref()?;
    Some(
        render_tool_call_template(tmpl, json_data, raw_fallback).unwrap_or_else(|e| {
            eprintln!(
                "⚠ template error in tool '{}' call_template: {e}",
                call.name
            );
            raw_fallback.to_string()
        }),
    )
}

/// Look up and render result template for a tool, returning rendered string or None.
/// Logs warning to stderr on render error.
fn render_result(
    call: &ToolCall,
    result: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    let tmpl = decl_map.get(&call.name)?.result_template.as_ref()?;
    Some(
        render_tool_result_template(tmpl, result, raw_fallback).unwrap_or_else(|e| {
            eprintln!(
                "⚠ template error in tool '{}' result_template: {e}",
                call.name
            );
            raw_fallback.to_string()
        }),
    )
}

/// Emit a tool call event with optional template rendering.
fn emit_tool_call_with_template(
    call: &ToolCall,
    json_data: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
) {
    use harnx_core::event::{AgentEvent, ToolEvent, ToolKind};

    let raw_fallback = match json_data {
        Value::Null => String::new(),
        _ => pretty_yaml_block(json_data),
    };

    let title = render_call(call, json_data, &raw_fallback, decl_map);

    let event = AgentEvent::Tool(ToolEvent::Started {
        id: call.id.clone().unwrap_or_default(),
        name: call.name.clone(),
        kind: ToolKind::Other,
        title,
        input: json_data.clone(),
        locations: Vec::new(),
    });

    if !harnx_core::sink::emit_agent_event(event) && *IS_STDOUT_TERMINAL {
        print_tool_call_fallback(call, json_data, decl_map, &raw_fallback);
    }
}

/// Fallback print for tool call when no sink is installed.
fn print_tool_call_fallback(
    call: &ToolCall,
    json_data: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
    raw_fallback: &str,
) {
    if let Some(rendered) = render_call(call, json_data, raw_fallback, decl_map) {
        println!("[tool] {} {}", call.name, rendered);
    } else {
        let text = if raw_fallback.is_empty() {
            format!("[tool] {}", call.name)
        } else {
            format!("[tool] {} {raw_fallback}", call.name)
        };
        println!("{text}");
    }
}

/// Emit a tool result event with optional template rendering.
fn emit_tool_result_with_template(
    call: &ToolCall,
    result: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
) {
    use harnx_core::event::{AgentEvent, ContentBlock, ToolEvent};

    let raw_fallback = extract_user_display_text(result).unwrap_or_else(|| match result {
        Value::String(s) => s.clone(),
        _ => pretty_yaml_block(result),
    });

    let content = render_result(call, result, &raw_fallback, decl_map)
        .map(|text| vec![ContentBlock::Text(text)])
        .unwrap_or_default();

    let event = AgentEvent::Tool(ToolEvent::Completed {
        id: call.id.clone().unwrap_or_default(),
        output: result.clone(),
        content,
    });

    if !harnx_core::sink::emit_agent_event(event) && *IS_STDOUT_TERMINAL {
        print_tool_result_fallback(call, result, decl_map, &raw_fallback);
    }
}

/// Fallback print for tool result when no sink is installed.
fn print_tool_result_fallback(
    call: &ToolCall,
    result: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
    raw_fallback: &str,
) {
    let output_str = render_result(call, result, raw_fallback, decl_map)
        .unwrap_or_else(|| raw_fallback.to_string());

    let mut opts = TruncateOpts::default();
    let marker = " [...] ";
    match crossterm::terminal::size() {
        Ok((cols, rows)) => {
            opts.head_lines = 5.max((rows / 2) as usize);
            opts.tail_lines = 0;
            opts.line_head_bytes = (cols as usize).saturating_sub(3 + marker.len());
            opts.line_tail_bytes = 0;
            opts.marker = Some(marker.to_string());
        }
        Err(e) => {
            // Terminal size unavailable (e.g. CI, piped output) — use safe defaults
            eprintln!("debug: failed to get terminal size: {e}");
            opts.head_lines = 20;
            opts.tail_lines = 0;
            opts.line_head_bytes = 200;
            opts.line_tail_bytes = 0;
            opts.marker = Some(marker.to_string());
        }
    }
    let truncated = truncate_output(&output_str, &opts);
    let text = format!("{}\n", dimmed_text(&truncated));
    print!("{text}");
}

fn default_confirm_tool_use(tool_name: &str, arguments: &Value, reason: Option<&str>) -> bool {
    harnx_hooks::prompt::confirm_tool_use(tool_name, arguments, reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use indexmap::IndexMap;
    use parking_lot::RwLock;
    use serde_json::json;
    use std::sync::Arc;

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
        let result = eval_tool_calls(
            &build_tool_eval_context(&config, None),
            calls,
            &abort_signal,
        )
        .await
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].call.name, "unknown_tool");
        assert!(result[0].output.is_object());
        assert_eq!(result[0].output["is_error"], true);
        assert!(result[0].output["error"]
            .as_str()
            .unwrap()
            .contains("No tool provider configured"));
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
