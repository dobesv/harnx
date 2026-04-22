use crate::{
    config::GlobalConfig,
    hooks::HookEvent,
    mcp_safety::{truncate_output, TruncateOpts},
    utils::*,
};

use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

use harnx_core::tool::ToolProvider;
pub use harnx_core::tool::{
    extract_user_display_text, trigger_agent_tool_declaration, JsonSchema, SwitchAgentData,
    ToolCall, ToolDeclaration, ToolResult, Tools, TRIGGER_AGENT_TOOL_NAME,
};
pub use harnx_engine::tool::{
    eval_tool_calls, ConfirmToolUseFn, DispatchHookFn, ToolCallEmitFn, ToolEvalContext,
};

/// Build a `ToolEvalContext` from the harnx `GlobalConfig`. Replaces the
/// old inherent `ToolEvalContext::from_config` method — the struct lives
/// in `harnx-engine::tool` now (orphan rules forbid adding inherent
/// methods on a cross-crate type). Snapshots Config fields, constructs
/// the provider list (ACP first, MCP second), builds the dispatch hook
/// closure over captured `hooks.entries`, `session_id`, and `cwd`, and
/// wires in harnx-side default UI/prompt callbacks.
pub fn build_tool_eval_context(config: &GlobalConfig) -> ToolEvalContext {
    let guard = config.read();
    let use_tools = guard.extract_agent().use_tools().map(|v| v.join(","));
    let allowed_tool_names: HashSet<String> = guard
        .tool_declarations_for_use_tools(use_tools.as_deref())
        .into_iter()
        .map(|decl| decl.name)
        .collect();
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
            crate::hooks::dispatch::dispatch_hooks(&event, &hooks_entries, &session_id, &cwd).await
        })
    });

    let emit_tool_call_fn: Arc<ToolCallEmitFn> = Arc::new(default_emit_tool_call);
    let emit_tool_result_fn: Arc<ToolCallEmitFn> = Arc::new(default_emit_tool_result);
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

fn default_emit_tool_call(call: &ToolCall, json_data: &Value) {
    use harnx_core::event::{AgentEvent, ToolEvent, ToolKind};
    let event = AgentEvent::Tool(ToolEvent::Started {
        id: call.id.clone().unwrap_or_default(),
        name: call.name.clone(),
        kind: ToolKind::Other,
        title: None,
        input: json_data.clone(),
        locations: Vec::new(),
    });
    if !crate::agent_event_sink::emit_agent_event(event) && *IS_STDOUT_TERMINAL {
        // No sink installed (test context) — keep a visible fallback so
        // the batch doesn't go dark.
        let input_yaml = match json_data {
            Value::Null => None,
            _ => Some(pretty_yaml_block(json_data)),
        };
        let text = match input_yaml {
            Some(yaml) => format!("[tool] {} {yaml}", call.name),
            None => format!("[tool] {}", call.name),
        };
        println!("{text}");
    }
}

fn default_emit_tool_result(call: &ToolCall, result: &Value) {
    use harnx_core::event::{AgentEvent, ToolEvent};
    let event = AgentEvent::Tool(ToolEvent::Completed {
        id: call.id.clone().unwrap_or_default(),
        output: result.clone(),
        content: Vec::new(),
    });
    if !crate::agent_event_sink::emit_agent_event(event) && *IS_STDOUT_TERMINAL {
        // No sink installed — fall back to the old format-and-print path.
        let mut opts = TruncateOpts::default();
        let marker = " [...] ";
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            opts.head_lines = 5.max((rows / 2) as usize);
            opts.tail_lines = 0;
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
        print!("{text}");
    }
}

fn default_confirm_tool_use(tool_name: &str, arguments: &Value, reason: Option<&str>) -> bool {
    crate::hooks::prompt::confirm_tool_use(tool_name, arguments, reason)
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
        let result =
            eval_tool_calls(&build_tool_eval_context(&config), calls, &abort_signal).unwrap();
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
            eval_tool_calls(&build_tool_eval_context(&config), calls, &abort_signal).unwrap();
        assert_eq!(result.len(), 2);

        assert_eq!(result[0].call.name, TRIGGER_AGENT_TOOL_NAME);
        assert_eq!(result[0].output["action"], "switch_agent");

        assert_eq!(result[1].call.name, "unknown_tool");
        assert_eq!(result[1].output["is_error"], true);
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
