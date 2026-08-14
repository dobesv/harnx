use crate::{
    config::{Config, GlobalConfig},
    nats_hook_provider::{
        dispatch_hook_event, HookDispatchMeta, HookEventDispatch, NatsHookProvider,
    },
    utils::*,
};
use anyhow::Result;
use harnx_hooks::HookEvent;

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use harnx_core::tool::ToolProvider;
pub use harnx_core::tool::{
    extract_user_display_text, render_tool_call_template, render_tool_result_template, JsonSchema,
    SwitchAgentData, ToolCall, ToolDeclaration, ToolResult, Tools,
};
use harnx_engine::tool::ToolEvalRenderContext;
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

use crate::nats_tool_provider::NatsToolProvider;
use crate::tool_context::{discover_nats_hook_provider_cached, discover_nats_tool_provider_cached};
pub use crate::tool_context::{BuildToolEvalContextParams, ToolRoundParams};

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
    params: ToolRoundParams<'_>,
    tool_calls: Vec<ToolCall>,
) -> Result<Vec<ToolResult>> {
    execute_tool_round_with_persistence(params, tool_calls, ToolRoundPersistence::DEFAULT).await
}

pub async fn execute_tool_round_with_persistence(
    params: ToolRoundParams<'_>,
    tool_calls: Vec<ToolCall>,
    persistence: ToolRoundPersistence,
) -> Result<Vec<ToolResult>> {
    let ToolRoundParams {
        config,
        instance_id,
        input,
        completion,
        abort_signal,
        working_dir,
        nats_hook_provider,
        pending_async_context,
    } = params;
    let dry_run = config.read().dry_run;

    if persistence.persist_tool_calls && !dry_run {
        config.write().append_session_tool_calls(
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
    let eval_ctx = build_tool_eval_context(BuildToolEvalContextParams {
        config,
        instance_id,
        agent_use_tools: agent_use_tools.as_deref(),
        current_agent_package,
        working_dir,
        nats_hook_provider,
        pending_async_context,
    })
    .await;
    let results = match eval_tool_calls(&eval_ctx, tool_calls.clone(), abort_signal).await {
        Ok(results) => results,
        Err(err) => {
            if ToolApprovalInterrupt::from_error(&err).is_some() {
                return Err(err);
            }
            if !dry_run {
                persist_failed_tool_results(config, tool_calls, &eval_ctx, &err)?;
            }
            return Err(err);
        }
    };
    let results = populate_result_markdown(results, &eval_ctx);
    if !dry_run {
        config.write().append_session_tool_results(&results)?;
    }
    Ok(results)
}

fn persist_failed_tool_results(
    config: &GlobalConfig,
    tool_calls: Vec<ToolCall>,
    eval_ctx: &ToolEvalContext,
    error: &anyhow::Error,
) -> Result<()> {
    let fallback = tool_calls
        .into_iter()
        .map(|call| {
            ToolResult::new(
                call,
                serde_json::json!({
                    "error": format!("tool execution failed: {error:#}")
                }),
            )
        })
        .collect();
    let fallback = populate_result_markdown(fallback, eval_ctx);
    config
        .write()
        .append_session_tool_results(&fallback)
        .map_err(|persist_error| {
            anyhow::anyhow!(
                "{error:#}; additionally failed to persist fallback tool results: {persist_error:#}"
            )
        })
}

fn build_dispatch_hook_fn(
    session_name: Option<&str>,
    working_dir: Option<&std::path::Path>,
    nats_hook_provider: Option<Arc<NatsHookProvider>>,
    pending_async_context: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
) -> Arc<DispatchHookFn> {
    let session_id = session_name.unwrap_or("cmd").to_string();
    let cwd = working_dir
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    Arc::new(move |event: HookEvent| {
        let session_id = session_id.clone();
        let cwd = cwd.clone();
        let nats_hook_provider = nats_hook_provider.clone();
        let pending_async_context = pending_async_context.clone();
        Box::pin(async move {
            // Ask is returned unchanged. eval_tool_calls turns a deferred
            // confirmation into ToolApprovalRequiredError. Headless workers
            // cannot prompt, so their confirmation callback decides whether
            // the error is surfaced or the call is denied.
            dispatch_hook_event(HookEventDispatch {
                event,
                provider: nats_hook_provider.as_deref(),
                meta: HookDispatchMeta {
                    session_id,
                    cwd,
                    resume_count: 0,
                },
                pending_async_context,
            })
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

async fn resolve_nats_providers(
    config: &Config,
    instance_id: &harnx_core::instance::ServerScope,
    active_package: Option<&str>,
    injected_hook_provider: Option<Arc<NatsHookProvider>>,
) -> (Option<Arc<NatsToolProvider>>, Option<Arc<NatsHookProvider>>) {
    let tool_provider =
        discover_nats_tool_provider_cached(config, instance_id, active_package).await;
    let hook_provider = match injected_hook_provider {
        Some(provider) => Some(provider),
        None => discover_nats_hook_provider_cached(config, instance_id).await,
    };
    (tool_provider, hook_provider)
}

/// Build tool providers, hook dispatch, and rendering state for one tool round.
pub async fn build_tool_eval_context(params: BuildToolEvalContextParams<'_>) -> ToolEvalContext {
    let BuildToolEvalContextParams {
        config,
        instance_id,
        agent_use_tools,
        current_agent_package,
        working_dir,
        nats_hook_provider,
        pending_async_context,
    } = params;
    let (
        mut tool_declarations,
        handoff_targets,
        session_name,
        confirm_tool_use_fn,
        config_snapshot,
    ) = {
        let guard = config.read();
        let (tool_declarations, handoff_targets) = guard
            .tool_declarations_for_use_tools(agent_use_tools, current_agent_package.as_deref());
        (
            tool_declarations,
            handoff_targets,
            guard.session.as_ref().map(|s| s.id().to_string()),
            build_confirm_tool_use_fn(&guard),
            guard.clone(),
        )
    };

    let (nats_provider, nats_hook_provider) = resolve_nats_providers(
        &config_snapshot,
        instance_id,
        current_agent_package.as_deref(),
        nats_hook_provider,
    )
    .await;
    if let Some(provider) = &nats_provider {
        tool_declarations.extend(provider.declarations_for_use_tools(agent_use_tools));
    }

    let decl_map = Arc::new(build_decl_map(tool_declarations));
    let allowed_tool_names: HashSet<String> = decl_map.keys().cloned().collect();
    let providers = build_tool_providers(config, nats_provider);
    let dispatch_hook_fn = build_dispatch_hook_fn(
        session_name.as_deref(),
        working_dir,
        nats_hook_provider,
        pending_async_context,
    );
    let (emit_tool_call_fn, emit_tool_result_fn, emit_tool_blocked_fn) = build_emit_fns(&decl_map);
    ToolEvalContext {
        instance_id: instance_id.clone(),
        render: Some(ToolEvalRenderContext {
            decl_map: Arc::clone(&decl_map),
        }),
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

fn build_decl_map(tool_declarations: Vec<ToolDeclaration>) -> HashMap<String, ToolDeclaration> {
    tool_declarations
        .into_iter()
        .map(|declaration| (declaration.name.clone(), declaration))
        .collect()
}

fn build_confirm_tool_use_fn(config: &Config) -> Arc<ConfirmToolUseFn> {
    // Runtime-only TUI confirmation override (falls back to inquire prompt).
    config
        .tui_confirm_tool_use
        .clone()
        .unwrap_or_else(|| Arc::new(default_confirm_tool_use))
}

fn build_tool_providers(
    config: &GlobalConfig,
    nats_provider: Option<Arc<crate::nats_tool_provider::NatsToolProvider>>,
) -> Vec<Arc<dyn ToolProvider>> {
    let mut providers: Vec<Arc<dyn ToolProvider>> = Vec::new();
    // NATS is the runtime provider for configured sub-agent and tool-server tools.
    if let Some(nats) = nats_provider {
        providers.push(nats as Arc<dyn ToolProvider>);
    }
    providers.push(
        Arc::new(crate::session_history::SessionHistoryProvider::new(
            config.clone(),
        )) as Arc<dyn ToolProvider>,
    );
    providers
}

fn populate_result_markdown(
    results: Vec<ToolResult>,
    eval_ctx: &ToolEvalContext,
) -> Vec<ToolResult> {
    results
        .into_iter()
        .map(|mut result| {
            let raw_fallback = tool_result_raw_fallback(&result.output);
            result.markdown = eval_ctx.render.as_ref().and_then(|render| {
                render_result_for_display(
                    &result.call,
                    &result.output,
                    &raw_fallback,
                    render.decl_map.as_ref(),
                )
            });
            result
        })
        .collect()
}

fn tool_result_raw_fallback(output: &Value) -> String {
    extract_user_display_text(output).unwrap_or_else(|| match output {
        Value::String(text) => text.clone(),
        _ => pretty_yaml_block(output),
    })
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
        markdown: markdown.clone(),
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

    #[tokio::test]
    async fn instance_id_is_preserved_in_tool_eval_context() {
        let config = Arc::new(RwLock::new(Config::default()));
        let instance_id = harnx_core::instance::ServerScope::new();

        let context =
            build_tool_eval_context(BuildToolEvalContextParams::new(&config, &instance_id)).await;

        assert_eq!(context.instance_id, instance_id);
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
        let result = eval_tool_calls(
            &build_tool_eval_context(BuildToolEvalContextParams::new(
                &config,
                &harnx_core::instance::ServerScope::new(),
            ))
            .await,
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

        // Mirror the derivation done in `execute_tool_round`.
        let pkg = harnx_core::package_namespace::pkg_from_qualified("pantheon/daedalus")
            .map(str::to_string);
        assert_eq!(pkg.as_deref(), Some("pantheon"));
        let ctx = build_tool_eval_context(
            BuildToolEvalContextParams::new(&config, &harnx_core::instance::ServerScope::new())
                .with_current_agent_package(pkg),
        )
        .await;
        assert_eq!(ctx.current_agent_package.as_deref(), Some("pantheon"));

        // A bare (top-level) agent name yields no package context.
        let bare =
            harnx_core::package_namespace::pkg_from_qualified("daedalus").map(str::to_string);
        assert_eq!(bare, None);
        let ctx = build_tool_eval_context(
            BuildToolEvalContextParams::new(&config, &harnx_core::instance::ServerScope::new())
                .with_current_agent_package(bare),
        )
        .await;
        assert_eq!(ctx.current_agent_package, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn build_tool_eval_context_uses_tui_confirm_override() {
        // When the TUI installs a confirmation callback, the eval context must
        // use it instead of the default inquire prompt (#695).
        let _guard = crate::client::TestStateGuard::new(None).await;
        let config = Arc::new(RwLock::new(Config::default()));

        // Default (no override): the inquire-based prompt is used. In a
        // non-terminal test process it denies, so this returns Deny.
        let ctx = build_tool_eval_context(BuildToolEvalContextParams::new(
            &config,
            &harnx_core::instance::ServerScope::new(),
        ))
        .await;
        let call = ToolCall::new("t".to_string(), serde_json::json!({}), None, None);
        assert!(matches!(
            (ctx.confirm_tool_use_fn)(&call, &serde_json::json!({}), None),
            ToolUseConfirmation::Deny { .. }
        ));

        // With an override installed, the context routes confirmation through it.
        config
            .write()
            .set_tui_confirm_tool_use(Some(Arc::new(|_, _, _| ToolUseConfirmation::Approve)));
        let ctx = build_tool_eval_context(BuildToolEvalContextParams::new(
            &config,
            &harnx_core::instance::ServerScope::new(),
        ))
        .await;
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

    use harnx_core::event::{AgentEvent, AgentEventSink, ToolEvent};
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct RecordingSink {
        events: StdMutex<Vec<AgentEvent>>,
    }
    impl AgentEventSink for RecordingSink {
        fn emit(&self, event: AgentEvent) {
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

    #[test]
    fn populate_result_markdown_renders_templates_and_leaves_missing_templates_none() {
        let mut decl_map = HashMap::new();
        decl_map.insert(
            "bash_exec".to_string(),
            make_decl_with_templates("bash_exec", None, Some("OK: {{ result.content[0].text }}")),
        );
        let eval_ctx = ToolEvalContext {
            instance_id: harnx_core::instance::ServerScope::new(),
            render: Some(ToolEvalRenderContext {
                decl_map: Arc::new(decl_map),
            }),
            providers: Vec::new(),
            session_name: None,
            allowed_tool_names: HashSet::new(),
            current_agent_package: None,
            handoff_targets: HashMap::new(),
            emit_tool_call_fn: Arc::new(|_, _| {}),
            emit_tool_result_fn: Arc::new(|_, _| {}),
            emit_tool_blocked_fn: Arc::new(|_, _| {}),
            confirm_tool_use_fn: Arc::new(|_, _, _| ToolUseConfirmation::Approve),
            dispatch_hook_fn: Arc::new(|_| {
                Box::pin(async {
                    harnx_core::hooks::HookOutcome {
                        control: harnx_core::hooks::HookResultControl::Continue,
                        result: harnx_core::hooks::HookResult::default(),
                    }
                })
            }),
        };

        let results = populate_result_markdown(
            vec![
                ToolResult::new(
                    ToolCall::new(
                        "bash_exec".to_string(),
                        json!({"command": "echo hi"}),
                        Some("call-1".to_string()),
                        None,
                    ),
                    json!({"content": [{"type": "text", "text": "hello"}]}),
                ),
                ToolResult::new(
                    ToolCall::new(
                        "plain_tool".to_string(),
                        json!({"command": "echo hi"}),
                        Some("call-2".to_string()),
                        None,
                    ),
                    json!({"content": [{"type": "text", "text": "raw"}]}),
                ),
            ],
            &eval_ctx,
        );

        assert_eq!(results[0].markdown.as_deref(), Some("OK: hello"));
        assert!(results[1].markdown.is_none());
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
}
