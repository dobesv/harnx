use crate::{
    config::{GlobalConfig, Input},
    utils::*,
};
use anyhow::Result;
use harnx_core::hooks::HookConfig;
use harnx_hooks::{HookEvent, PersistentHookManager};

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use harnx_core::tool::ToolProvider;
pub use harnx_core::tool::{
    extract_user_display_text, render_tool_call_template, render_tool_result_template, JsonSchema,
    SwitchAgentData, ToolCall, ToolDeclaration, ToolResult, Tools,
};
pub use harnx_engine::tool::{
    eval_tool_calls, ConfirmToolUseFn, DeferredToolCall, DispatchHookFn, ToolApprovalRequiredError,
    ToolCallEmitFn, ToolEvalContext, ToolUseConfirmation,
};

/// The LLM text completion that immediately preceded a tool round.
/// Groups the assistant output text and optional chain-of-thought together
/// so `execute_tool_round` doesn't need separate `output` and `thought` args.
pub struct CompletionText<'a> {
    pub output: &'a str,
    pub thought: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct ToolApprovalInterrupt {
    pub tool_calls: Vec<ToolCall>,
    pub deferred_calls: Vec<DeferredToolCall>,
}

#[derive(Debug, Clone, Copy)]
pub struct ToolRoundPersistence {
    pub persist_tool_calls: bool,
}

impl ToolRoundPersistence {
    pub const DEFAULT: Self = Self {
        persist_tool_calls: true,
    };

    pub const REUSE_EXISTING_CALLS: Self = Self {
        persist_tool_calls: false,
    };
}

impl ToolApprovalInterrupt {
    pub fn from_error(err: &anyhow::Error) -> Option<Self> {
        err.downcast_ref::<ToolApprovalRequiredError>()
            .map(|typed| Self {
                tool_calls: typed.tool_calls().to_vec(),
                deferred_calls: typed.deferred_calls().to_vec(),
            })
    }
}

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
    completion: &CompletionText<'_>,
    tool_calls: Vec<ToolCall>,
    abort_signal: &AbortSignal,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    working_dir: Option<&std::path::Path>,
) -> Result<Vec<ToolResult>> {
    execute_tool_round_with_persistence(
        config,
        input,
        completion,
        tool_calls,
        abort_signal,
        persistent_manager,
        working_dir,
        ToolRoundPersistence::DEFAULT,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_round_with_persistence(
    config: &GlobalConfig,
    input: &Input,
    completion: &CompletionText<'_>,
    tool_calls: Vec<ToolCall>,
    abort_signal: &AbortSignal,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    working_dir: Option<&std::path::Path>,
    persistence: ToolRoundPersistence,
) -> Result<Vec<ToolResult>> {
    let dry_run = config.read().dry_run;

    if persistence.persist_tool_calls && !dry_run {
        config.write().save_session_tool_calls(
            input,
            completion.output,
            completion.thought,
            &tool_calls,
        )?;
    }

    let agent_use_tools = input.agent().use_tools().map(|v| v.join(","));
    // Derive the active agent's package (e.g. `pantheon` for `pantheon/daedalus`)
    // so bare `_session_handoff` targets resolve to the same package (#709).
    let current_agent_package =
        harnx_core::package_namespace::pkg_from_qualified(input.agent().name()).map(str::to_string);
    let eval_ctx = build_tool_eval_context(
        config,
        agent_use_tools.as_deref(),
        current_agent_package,
        persistent_manager,
        working_dir,
    );
    let results = match eval_tool_calls(&eval_ctx, tool_calls.clone(), abort_signal).await {
        Ok(results) => results,
        Err(err) => {
            if ToolApprovalInterrupt::from_error(&err).is_some() {
                return Err(err);
            }
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
/// Build the hook dispatch closure, capturing hooks config and the persistent
/// manager so `ToolEvalContext` can fire PreToolUse/PostToolUse hooks.
fn build_dispatch_hook_fn(
    hooks: &harnx_hooks::HooksConfig,
    // Value is (bare_server_tool_name, hook_entries). The matcher in each entry is
    // evaluated against the bare name, not the prefixed display name, so renaming
    // an MCP server doesn't require updating hook matchers.
    per_tool_hooks: HashMap<String, (String, Vec<HookConfig>)>,
    session_name: Option<&str>,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    working_dir: Option<&std::path::Path>,
) -> Arc<DispatchHookFn> {
    let hooks_entries = hooks.entries.clone();
    let session_id = session_name.unwrap_or("cmd").to_string();
    let cwd = working_dir
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let persistent_manager = persistent_manager.clone();
    Arc::new(move |event: HookEvent| {
        let hooks_entries = hooks_entries.clone();
        let per_tool_hooks = per_tool_hooks.clone();
        let session_id = session_id.clone();
        let cwd = cwd.clone();
        let persistent_manager = persistent_manager.clone();
        Box::pin(async move {
            let display_tool_name = match &event {
                HookEvent::PreToolUse { tool_name, .. }
                | HookEvent::PostToolUse { tool_name, .. }
                | HookEvent::PostToolUseFailure { tool_name, .. } => Some(tool_name.as_str()),
                _ => None,
            };

            // For server-scoped hooks, match the hook's `matcher` against the bare
            // (unprefixed) tool name. Strip the matcher from entries we add to the
            // merged list so the global dispatcher doesn't re-check against the
            // prefixed display name and accidentally reject them.
            let mut merged_entries: Vec<HookConfig> = if let Some(display_name) = display_tool_name
            {
                per_tool_hooks
                    .get(display_name)
                    .map(|(bare_name, entries)| {
                        entries
                            .iter()
                            .filter(|hook| {
                                harnx_hooks::CompiledMatcher::compile(&hook.matcher)
                                    .map(|m| m.matches_str(bare_name))
                                    .unwrap_or(false)
                            })
                            .map(|hook| HookConfig {
                                matcher: None,
                                ..hook.clone()
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            merged_entries.extend(hooks_entries.clone());

            harnx_hooks::dispatch::dispatch_hooks_with_count_and_manager(
                &event,
                &merged_entries,
                &session_id,
                &cwd,
                0,
                None,
                Some(&persistent_manager),
            )
            .await
        })
    })
}

/// Build the three emit closures (call / result / blocked) over a shared decl map.
fn build_emit_fns(
    decl_map: &Arc<HashMap<String, ToolDeclaration>>,
) -> (
    Arc<ToolCallEmitFn>,
    Arc<ToolCallEmitFn>,
    Arc<ToolCallEmitFn>,
) {
    let m1 = Arc::clone(decl_map);
    let emit_tool_call_fn: Arc<ToolCallEmitFn> =
        Arc::new(move |call: &ToolCall, json_data: &Value| {
            emit_tool_call_with_template(call, json_data, &m1);
        });
    let m2 = Arc::clone(decl_map);
    let emit_tool_result_fn: Arc<ToolCallEmitFn> =
        Arc::new(move |call: &ToolCall, result: &Value| {
            emit_tool_result_with_template(call, result, &m2);
        });
    let m3 = Arc::clone(decl_map);
    let emit_tool_blocked_fn: Arc<ToolCallEmitFn> =
        Arc::new(move |call: &ToolCall, blocked_result: &Value| {
            emit_tool_blocked_with_template(call, blocked_result, &m3);
        });
    (emit_tool_call_fn, emit_tool_result_fn, emit_tool_blocked_fn)
}

pub fn build_tool_eval_context(
    config: &GlobalConfig,
    agent_use_tools: Option<&str>,
    current_agent_package: Option<String>,
    persistent_manager: &std::sync::Arc<tokio::sync::Mutex<PersistentHookManager>>,
    working_dir: Option<&std::path::Path>,
) -> ToolEvalContext {
    let guard = config.read();
    let (tool_declarations, handoff_targets) =
        guard.tool_declarations_for_use_tools(agent_use_tools, current_agent_package.as_deref());
    let decl_map: Arc<HashMap<String, ToolDeclaration>> = Arc::new(
        tool_declarations
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect(),
    );
    let allowed_tool_names: HashSet<String> = decl_map.keys().cloned().collect();
    let hooks = guard.resolved_hooks();
    let acp_manager = guard.acp_manager.clone();
    let mcp_manager = guard.mcp_manager.clone();
    // Build a map from display tool name → (bare_tool_name, server_hook_entries).
    // Look up hooks via the McpManager (which stores clients keyed by display name),
    // so packaged/prefixed servers are found correctly. Server hooks are filtered to
    // only tool-use events; their matchers are evaluated against the bare name so
    // renaming a server doesn't require updating hook matchers.
    let per_tool_hooks: HashMap<String, (String, Vec<HookConfig>)> = decl_map
        .iter()
        .filter_map(|(tool_name, decl)| {
            let server_name = decl.mcp_server_name.as_ref()?;
            let bare_name = decl
                .mcp_tool_name
                .clone()
                .unwrap_or_else(|| tool_name.clone());
            let server_hooks = mcp_manager
                .as_ref()?
                .get_client(server_name)
                .and_then(|client| client.hooks().cloned())?;
            let hook_entries = server_hooks
                .entries
                .iter()
                .filter(|hook| {
                    matches!(
                        hook.event.as_str(),
                        "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
                    )
                })
                .cloned()
                .collect::<Vec<_>>();
            (!hook_entries.is_empty()).then(|| (tool_name.clone(), (bare_name, hook_entries)))
        })
        .collect();
    let session_name = guard.session.as_ref().map(|s| s.id().to_string());
    // Runtime-only TUI confirmation override (falls back to the inquire prompt).
    let confirm_tool_use_fn: Arc<ConfirmToolUseFn> = guard
        .tui_confirm_tool_use
        .clone()
        .unwrap_or_else(|| Arc::new(default_confirm_tool_use));
    drop(guard);

    // Build the provider list in ACP-first order so ACP sub-agent
    // handoffs take priority over any namespaced MCP tool with the same name.
    let mut providers: Vec<Arc<dyn ToolProvider>> = Vec::new();
    if let Some(acp) = acp_manager {
        providers.push(acp as Arc<dyn ToolProvider>);
    }
    if let Some(mcp) = mcp_manager {
        providers.push(mcp as Arc<dyn ToolProvider>);
    }
    providers.push(
        Arc::new(crate::session_history::SessionHistoryProvider::new(
            config.clone(),
        )) as Arc<dyn ToolProvider>,
    );

    let dispatch_hook_fn = build_dispatch_hook_fn(
        &hooks,
        per_tool_hooks,
        session_name.as_deref(),
        persistent_manager,
        working_dir,
    );
    let (emit_tool_call_fn, emit_tool_result_fn, emit_tool_blocked_fn) = build_emit_fns(&decl_map);

    ToolEvalContext {
        providers,
        session_name,
        allowed_tool_names,
        current_agent_package,
        handoff_targets,
        emit_tool_call_fn,
        emit_tool_result_fn,
        emit_tool_blocked_fn,
        confirm_tool_use_fn,
        dispatch_hook_fn,
    }
}

/// Look up and render the call template for a tool, returning rendered string or None.
/// On template render error, logs a warning via `log::warn!` and falls back to
/// `raw_fallback` so display continues uninterrupted.
fn render_call(
    call: &ToolCall,
    json_data: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    let tmpl = decl_map.get(&call.name)?.call_template.as_ref()?;
    Some(
        render_tool_call_template(tmpl, json_data, raw_fallback).unwrap_or_else(|e| {
            log::warn!("template error in tool '{}' call_template: {e}", call.name);
            raw_fallback.to_string()
        }),
    )
}

/// Look up and render result template for a tool, returning rendered string or None.
/// On template render error, logs a warning via `log::warn!` and falls back to
/// `raw_fallback` so display continues uninterrupted.
fn render_result(
    call: &ToolCall,
    result: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    let tmpl = decl_map.get(&call.name)?.result_template.as_ref()?;
    Some(
        render_tool_result_template(tmpl, result, raw_fallback).unwrap_or_else(|e| {
            log::warn!(
                "template error in tool '{}' result_template: {e}",
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

    let markdown = render_call(call, json_data, &raw_fallback, decl_map);

    let event = AgentEvent::Tool(ToolEvent::Started {
        id: call.id.clone().unwrap_or_default(),
        name: call.name.clone(),
        kind: ToolKind::Other,
        markdown,
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
    use harnx_core::event::{AgentEvent, ToolEvent};

    let raw_fallback = extract_user_display_text(result).unwrap_or_else(|| match result {
        Value::String(s) => s.clone(),
        _ => pretty_yaml_block(result),
    });

    let markdown = render_result(call, result, &raw_fallback, decl_map);

    let event = AgentEvent::Tool(ToolEvent::Completed {
        id: call.id.clone().unwrap_or_default(),
        output: result.clone(),
        markdown,
    });

    if !harnx_core::sink::emit_agent_event(event) && *IS_STDOUT_TERMINAL {
        print_tool_result_fallback(call, result, decl_map, &raw_fallback);
    }
}

fn emit_tool_blocked_with_template(
    call: &ToolCall,
    blocked_result: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
) {
    use harnx_core::event::{AgentEvent, ToolEvent};

    let reason = blocked_result
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("blocked by hook")
        .to_string();

    let raw_fallback = match &call.arguments {
        Value::Null => String::new(),
        _ => pretty_yaml_block(&call.arguments),
    };
    let input_rendered = render_call(call, &call.arguments, &raw_fallback, decl_map);

    let event = AgentEvent::Tool(ToolEvent::Blocked {
        id: call.id.clone().unwrap_or_default(),
        name: call.name.clone(),
        input: call.arguments.clone(),
        reason,
    });
    let _ = input_rendered;
    let _ = harnx_core::sink::emit_agent_event(event);
}

/// Look up `call.name` in `decl_map`, render `call_template` against `json_data`
/// using `raw_fallback`. Returns `Some(rendered_markdown)` or `None`.
pub fn render_call_for_display(
    call: &ToolCall,
    json_data: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    render_call(call, json_data, raw_fallback, decl_map)
}

/// Look up `call.name` in `decl_map`, render `result_template` against `result`
/// using `raw_fallback`. Returns `Some(rendered_markdown)` or `None`.
pub fn render_result_for_display(
    call: &ToolCall,
    result: &Value,
    raw_fallback: &str,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Option<String> {
    render_result(call, result, raw_fallback, decl_map)
}

/// Fallback print for tool result when no sink is installed. Routes
/// through the shared `render_tool_result_text` helper so this no-sink
/// path stays consistent with what the TUI/CLI sinks render.
fn print_tool_result_fallback(
    call: &ToolCall,
    result: &Value,
    decl_map: &HashMap<String, ToolDeclaration>,
    raw_fallback: &str,
) {
    let markdown = render_result(call, result, raw_fallback, decl_map);
    let truncated = render_tool_result_text(result, markdown.as_deref());
    println!("{}", dimmed_text(&truncated));
}

fn default_confirm_tool_use(
    call: &ToolCall,
    arguments: &Value,
    reason: Option<&str>,
) -> ToolUseConfirmation {
    if harnx_hooks::prompt::confirm_tool_use(&call.name, arguments, reason) {
        ToolUseConfirmation::Approve
    } else {
        ToolUseConfirmation::Deny {
            reason: reason.map(str::to_string),
        }
    }
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
        let persistent_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
            harnx_hooks::PersistentHookManager::new(),
        ));
        let result = eval_tool_calls(
            &build_tool_eval_context(&config, None, None, &persistent_manager, None),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn build_tool_eval_context_stores_current_agent_package() {
        // The package derived from a qualified agent name in `execute_tool_round`
        // must be threaded into `ToolEvalContext` so the engine can resolve
        // same-package handoff targets (#709).
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(RwLock::new(Config::default()));
        let persistent_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
            harnx_hooks::PersistentHookManager::new(),
        ));

        // Mirror the derivation done in `execute_tool_round`.
        let pkg = harnx_core::package_namespace::pkg_from_qualified("pantheon/daedalus")
            .map(str::to_string);
        assert_eq!(pkg.as_deref(), Some("pantheon"));
        let ctx = build_tool_eval_context(&config, None, pkg, &persistent_manager, None);
        assert_eq!(ctx.current_agent_package.as_deref(), Some("pantheon"));

        // A bare (top-level) agent name yields no package context.
        let bare =
            harnx_core::package_namespace::pkg_from_qualified("daedalus").map(str::to_string);
        assert_eq!(bare, None);
        let ctx = build_tool_eval_context(&config, None, bare, &persistent_manager, None);
        assert_eq!(ctx.current_agent_package, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_tool_eval_context_uses_tui_confirm_override() {
        // When the TUI installs a confirmation callback, the eval context must
        // use it instead of the default inquire prompt (#695).
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(RwLock::new(Config::default()));
        let persistent_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
            harnx_hooks::PersistentHookManager::new(),
        ));

        // Default (no override): the inquire-based prompt is used. In a
        // non-terminal test process it denies, so this returns Deny.
        let ctx = build_tool_eval_context(&config, None, None, &persistent_manager, None);
        let call = ToolCall::new("t".to_string(), serde_json::json!({}), None, None);
        assert!(matches!(
            (ctx.confirm_tool_use_fn)(&call, &serde_json::json!({}), None),
            ToolUseConfirmation::Deny { .. }
        ));

        // With an override installed, the context routes confirmation through it.
        config
            .write()
            .set_tui_confirm_tool_use(Some(Arc::new(|_, _, _| ToolUseConfirmation::Approve)));
        let ctx = build_tool_eval_context(&config, None, None, &persistent_manager, None);
        assert!(matches!(
            (ctx.confirm_tool_use_fn)(&call, &serde_json::json!({}), None),
            ToolUseConfirmation::Approve
        ));
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

    // ----------------------------------------------------------------
    // MCP MiniJinja templating, producer side: verify that
    // `emit_tool_call_with_template` and `emit_tool_result_with_template`
    // populate `markdown` on both `ToolEvent::Started` and
    // `ToolEvent::Completed` when the matching tool declaration carries
    // a `call_template` / `result_template`. These are the values the
    // TUI/CLI consumers then render — without these events being
    // populated, the consumer-side rendering has nothing to display.
    // ----------------------------------------------------------------

    use harnx_core::event::{AgentEvent, AgentEventSink, AgentSource, ToolEvent};
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct RecordingSink {
        events: StdMutex<Vec<AgentEvent>>,
    }
    impl AgentEventSink for RecordingSink {
        fn emit(&self, event: AgentEvent, _source: Option<AgentSource>) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn make_decl_with_templates(
        name: &str,
        call_template: Option<&str>,
        result_template: Option<&str>,
    ) -> ToolDeclaration {
        ToolDeclaration {
            name: name.to_string(),
            description: String::new(),
            parameters: Default::default(),
            mcp_tool_name: Some(name.to_string()),
            mcp_server_name: None,
            call_template: call_template.map(String::from),
            result_template: result_template.map(String::from),
            idempotent_hint: None,
            read_only_hint: None,
        }
    }

    /// Lock around the global sink so producer-side emit tests don't race
    /// with each other. The sink is process-global state. Ignore poisoning
    /// so a panic in one test doesn't cascade-fail every other test that
    /// touches the sink.
    fn with_recording_sink<R>(test_body: impl FnOnce(Arc<RecordingSink>) -> R) -> R {
        static TEST_LOCK: StdMutex<()> = StdMutex::new(());
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        harnx_core::sink::clear_agent_event_sink();
        let sink: Arc<RecordingSink> = Arc::new(RecordingSink::default());
        harnx_core::sink::install_agent_event_sink(sink.clone());
        let result = test_body(sink);
        harnx_core::sink::clear_agent_event_sink();
        result
    }

    #[test]
    fn render_call_for_display_returns_rendered_when_template_exists() {
        let decl = make_decl_with_templates("bash_exec", Some("$ {{ args.command }}"), None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({"command": "ls -la /tmp"}),
            Some("call-1".to_string()),
            None,
        );
        let json_data = json!({"command": "ls -la /tmp"});
        let raw_fallback = "command: ls -la /tmp";
        let result = render_call_for_display(&call, &json_data, raw_fallback, &decl_map);
        assert_eq!(result, Some("$ ls -la /tmp".to_string()));
    }

    #[test]
    fn emit_tool_call_with_template_sets_started_markdown() {
        let decl = make_decl_with_templates("bash_exec", Some("$ {{ args.command }}"), None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);

        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({"command": "ls -la"}),
            Some("call-1".to_string()),
            None,
        );
        let args = json!({"command": "ls -la"});

        with_recording_sink(|sink| {
            emit_tool_call_with_template(&call, &args, &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1, "expected one Started event");
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Started { markdown, name, .. }) => {
                    assert_eq!(name, "bash_exec");
                    assert_eq!(
                        markdown.as_deref(),
                        Some("$ ls -la"),
                        "template should be rendered into Started.markdown"
                    );
                }
                other => panic!("expected Started event, got {other:?}"),
            }
        });
    }

    #[test]
    fn emit_tool_call_without_template_leaves_markdown_none() {
        let decl = make_decl_with_templates("plain_tool", None, None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "plain_tool".to_string(),
            json!({"x": 1}),
            Some("call-1".to_string()),
            None,
        );
        let args = json!({"x": 1});

        with_recording_sink(|sink| {
            emit_tool_call_with_template(&call, &args, &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Started { markdown, .. }) => {
                    assert!(markdown.is_none(), "no template => markdown must be None");
                }
                other => panic!("expected Started event, got {other:?}"),
            }
        });
    }

    #[test]
    fn emit_tool_result_with_template_sets_completed_markdown() {
        let decl =
            make_decl_with_templates("bash_exec", None, Some("OK: {{ result.content[0].text }}"));
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);

        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({}),
            Some("call-1".to_string()),
            None,
        );
        let result_json = json!({
            "content": [{"type": "text", "text": "hello"}],
            "isError": false,
        });

        with_recording_sink(|sink| {
            emit_tool_result_with_template(&call, &result_json, &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1, "expected one Completed event");
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Completed { markdown, .. }) => {
                    assert_eq!(
                        markdown.as_deref(),
                        Some("OK: hello"),
                        "template should be rendered into Completed.markdown"
                    );
                }
                other => panic!("expected Completed event, got {other:?}"),
            }
        });
    }

    #[test]
    fn render_result_for_display_returns_none_when_no_template() {
        let decl = make_decl_with_templates("bash_exec", None, None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({"command": "ls"}),
            Some("call-1".to_string()),
            None,
        );
        let result_val = json!({"output": "file.txt"});
        let result = render_result_for_display(&call, &result_val, "raw fallback", &decl_map);
        assert!(result.is_none(), "no result_template => should return None");
    }

    // --- Server-scoped hook dispatch tests -----------------------------------
    //
    // These tests exercise `build_dispatch_hook_fn` directly to verify that
    // per-server hooks use bare-name matching and are correctly merged with
    // global hooks.

    fn make_hook_config(event: &str, matcher: Option<&str>, command: &str) -> HookConfig {
        HookConfig {
            event: event.to_string(),
            matcher: matcher.map(|s| s.to_string()),
            command: command.to_string(),
            timeout: Some(5),
            status_message: None,
            async_hook: None,
            hook_type: "claude-command".to_string(),
        }
    }

    #[cfg(unix)]
    fn pre_tool_use_event_with_name(tool_name: &str) -> HookEvent {
        HookEvent::PreToolUse {
            tool_name: tool_name.to_string(),
            tool_input: serde_json::json!({}),
            tool_use_id: "test-id".to_string(),
        }
    }

    fn session_start_event() -> HookEvent {
        HookEvent::SessionStart {
            source: "test".to_string(),
            model: "test-model".to_string(),
        }
    }

    fn make_per_tool_hooks(
        display_name: &str,
        bare_name: &str,
        hooks: Vec<HookConfig>,
    ) -> HashMap<String, (String, Vec<HookConfig>)> {
        let mut map = HashMap::new();
        map.insert(display_name.to_string(), (bare_name.to_string(), hooks));
        map
    }

    async fn dispatch_and_collect_context(
        global_hooks: Vec<HookConfig>,
        per_tool_hooks: HashMap<String, (String, Vec<HookConfig>)>,
        event: HookEvent,
    ) -> harnx_hooks::HookOutcome {
        use harnx_hooks::HooksConfig;

        let hooks_config = HooksConfig {
            max_resume: None,
            entries: global_hooks,
        };
        let pm = Arc::new(tokio::sync::Mutex::new(
            harnx_hooks::PersistentHookManager::new(),
        ));
        let dispatch_fn = build_dispatch_hook_fn(&hooks_config, per_tool_hooks, None, &pm, None);
        (dispatch_fn)(event).await
    }

    /// A no-matcher server hook applies to all tools on that server.
    #[cfg(unix)]
    #[tokio::test]
    async fn server_hook_no_matcher_matches_any_tool() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("hook.sh");
        let mut f = std::fs::File::create(&script_path).expect("create script");
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "cat > /dev/null").unwrap();
        writeln!(f, "echo '{{\"additionalContext\":\"server-hook-ran\"}}'").unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let server_hook = make_hook_config(
            "PreToolUse",
            None, // no matcher — should match all tools
            script_path.to_str().unwrap(),
        );
        let per_tool = make_per_tool_hooks(
            "myserver_exec", // display name
            "exec",          // bare name
            vec![server_hook],
        );
        let outcome = dispatch_and_collect_context(
            vec![],
            per_tool,
            pre_tool_use_event_with_name("myserver_exec"),
        )
        .await;
        assert!(
            outcome.result.additional_context.as_deref() == Some("server-hook-ran"),
            "no-matcher server hook should run for any tool on the server, got: {:?}",
            outcome.result.additional_context
        );
    }

    /// A server hook with `matcher: "exec"` matches the bare name `exec`,
    /// NOT the prefixed display name `myserver_exec`.
    #[cfg(unix)]
    #[tokio::test]
    async fn server_hook_matcher_uses_bare_name_not_display_name() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("hook.sh");
        let mut f = std::fs::File::create(&script_path).expect("create script");
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "cat > /dev/null").unwrap();
        writeln!(f, "echo '{{\"additionalContext\":\"bare-match\"}}'").unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // matcher = "^exec$" — matches bare name "exec" but NOT "myserver_exec"
        let server_hook =
            make_hook_config("PreToolUse", Some("^exec$"), script_path.to_str().unwrap());
        let per_tool = make_per_tool_hooks(
            "myserver_exec", // display name (what the event carries)
            "exec",          // bare name (what the matcher runs against)
            vec![server_hook],
        );
        let outcome = dispatch_and_collect_context(
            vec![],
            per_tool,
            pre_tool_use_event_with_name("myserver_exec"),
        )
        .await;
        assert_eq!(
            outcome.result.additional_context.as_deref(),
            Some("bare-match"),
            "matcher 'exec' should match bare name 'exec', even though display name is 'myserver_exec'"
        );
    }

    /// A server hook whose matcher doesn't match the bare name is excluded.
    #[cfg(unix)]
    #[tokio::test]
    async fn server_hook_matcher_excludes_non_matching_bare_name() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("hook.sh");
        let mut f = std::fs::File::create(&script_path).expect("create script");
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "cat > /dev/null").unwrap();
        writeln!(f, "echo '{{\"additionalContext\":\"should-not-run\"}}'").unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // matcher = "^list$" does NOT match bare name "exec"
        let server_hook =
            make_hook_config("PreToolUse", Some("^list$"), script_path.to_str().unwrap());
        let per_tool = make_per_tool_hooks("myserver_exec", "exec", vec![server_hook]);
        let outcome = dispatch_and_collect_context(
            vec![],
            per_tool,
            pre_tool_use_event_with_name("myserver_exec"),
        )
        .await;
        assert!(
            outcome.result.additional_context.is_none(),
            "hook with matcher '^list$' should not run for bare name 'exec'"
        );
    }

    /// Non-tool events (e.g. SessionStart) don't pick up server hooks
    /// because `per_tool_hooks` is keyed by display tool name.
    #[tokio::test]
    async fn server_hooks_not_applied_to_non_tool_events() {
        // No script needed — we just verify the hook doesn't fire.
        // A hook entry that would normally match is in per_tool_hooks, but
        // the event is SessionStart which yields no display_tool_name.
        let server_hook = make_hook_config("SessionStart", None, "echo hi");
        let per_tool = make_per_tool_hooks("myserver_exec", "exec", vec![server_hook]);
        let outcome = dispatch_and_collect_context(vec![], per_tool, session_start_event()).await;
        // SessionStart is not in the filtered tool-use events list so it
        // would have been stripped at build_tool_eval_context time, but
        // even if it were present, display_tool_name is None for non-tool
        // events so no per_tool_hooks lookup happens.
        assert!(outcome.result.additional_context.is_none());
    }

    #[test]
    fn emit_tool_result_without_template_leaves_markdown_none() {
        let decl = make_decl_with_templates("plain_tool", None, None);
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);
        let call = ToolCall::new(
            "plain_tool".to_string(),
            json!({}),
            Some("call-1".to_string()),
            None,
        );
        let result_json = json!({"content": [{"type": "text", "text": "hi"}]});

        with_recording_sink(|sink| {
            emit_tool_result_with_template(&call, &result_json, &decl_map);
            let events = sink.events.lock().unwrap();
            assert_eq!(events.len(), 1);
            match &events[0] {
                AgentEvent::Tool(ToolEvent::Completed { markdown, .. }) => {
                    assert!(
                        markdown.is_none(),
                        "no template => markdown must be None (consumer falls back to output)"
                    );
                }
                other => panic!("expected Completed event, got {other:?}"),
            }
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_dispatch_hook_fn_uses_explicit_working_dir_per_run() {
        use std::fs;
        use tempfile::TempDir;

        let hooks = harnx_hooks::HooksConfig {
            entries: vec![make_hook_config("SessionStart", None, "pwd > hook-cwd.txt")],
            max_resume: None,
        };
        let persistent_manager =
            std::sync::Arc::new(tokio::sync::Mutex::new(PersistentHookManager::new()));
        let root_a = TempDir::new().unwrap();
        let root_b = TempDir::new().unwrap();

        let dispatch_a = build_dispatch_hook_fn(
            &hooks,
            HashMap::new(),
            Some("session-a"),
            &persistent_manager,
            Some(root_a.path()),
        );
        let dispatch_b = build_dispatch_hook_fn(
            &hooks,
            HashMap::new(),
            Some("session-b"),
            &persistent_manager,
            Some(root_b.path()),
        );

        let (outcome_a, outcome_b) = tokio::join!(
            dispatch_a(session_start_event()),
            dispatch_b(session_start_event())
        );
        assert!(matches!(
            outcome_a.control,
            harnx_core::hooks::HookResultControl::Continue
        ));
        assert!(matches!(
            outcome_b.control,
            harnx_core::hooks::HookResultControl::Continue
        ));

        let cwd_a = fs::read_to_string(root_a.path().join("hook-cwd.txt")).unwrap();
        let cwd_b = fs::read_to_string(root_b.path().join("hook-cwd.txt")).unwrap();
        // Each hook's `pwd` is captured by the shell, so its exact string form
        // is platform/shell dependent (macOS resolves /var -> /private/var;
        // git-bash on Windows emits Unix-style /c/... paths that Rust's
        // fs::canonicalize can't resolve). Assert per-run isolation by the
        // TempDir's unique basename rather than full-path equality: each hook
        // ran in its own root, and the two dirs differ.
        let name_a = root_a.path().file_name().unwrap().to_str().unwrap();
        let name_b = root_b.path().file_name().unwrap().to_str().unwrap();
        assert!(
            cwd_a.trim().ends_with(name_a),
            "hook A cwd {cwd_a:?} should end with its root {name_a:?}"
        );
        assert!(
            cwd_b.trim().ends_with(name_b),
            "hook B cwd {cwd_b:?} should end with its root {name_b:?}"
        );
        assert_ne!(cwd_a, cwd_b);
    }
}
