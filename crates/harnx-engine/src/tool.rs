//! Tool-call loop — `eval_tool_calls` orchestrates a batch of tool
//! dispatches with pre/post hooks, user confirmation, UI emission,
//! and per-call abort handling. The loop is provider-agnostic: it
//! iterates `Vec<Arc<dyn ToolProvider>>` (harnx-core trait). All
//! harnx-specific concerns — UI rendering, hook dispatch execution,
//! inquire prompts — are injected via callbacks on `ToolEvalContext`,
//! constructed on the harnx side by `build_tool_eval_context`.

use anyhow::{anyhow, bail, Result};
use futures_util::future::join_all;
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::hooks::{HookEvent, HookOutcome, HookResult, HookResultControl};
use harnx_core::tool::{SwitchAgentData, ToolCall, ToolError, ToolProvider, ToolResult};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Callback invoked with a `&ToolCall` and the parsed arguments JSON.
/// Used for both "tool is about to dispatch" and "tool returned a
/// result" UI emission hooks on `ToolEvalContext`.
pub type ToolCallEmitFn = dyn Fn(&ToolCall, &Value) + Send + Sync;

/// Callback invoked when a PreToolUse hook returns `Ask { reason }`.
/// Receives the tool name, parsed arguments, and optional reason.
pub type ConfirmToolUseFn = dyn Fn(&str, &Value, Option<&str>) -> bool + Send + Sync;

/// Async callback used to dispatch hook events. Returns a `HookOutcome`
/// so callers can inspect `control` (Block/Ask/Continue) and any future
/// structured data carried in `result`.
pub type DispatchHookFn =
    dyn Fn(HookEvent) -> Pin<Box<dyn Future<Output = HookOutcome> + Send>> + Send + Sync;

pub struct ToolEvalContext {
    /// Ordered tool providers to search when dispatching a call.
    pub providers: Vec<Arc<dyn ToolProvider>>,
    /// Optional session name used when synthesizing `_session_handoff`
    /// results and the call omitted `session_id`.
    pub session_name: Option<String>,
    /// Allow-list of synthetic tool names that do not come from a real
    /// provider but are handled directly in `eval_tool_call_mcp`
    /// (currently `_session_handoff`).
    pub allowed_tool_names: HashSet<String>,
    /// Called when a tool is about to be dispatched. Receives the tool
    /// call and the parsed arguments JSON. Harnx's default emits an
    /// `AgentEvent::Tool(Started { .. })` via the unified AgentEvent
    /// sink, falling back to stdout if no sink is installed.
    pub emit_tool_call_fn: Arc<ToolCallEmitFn>,
    /// Called when a tool call returns a result. Receives the tool
    /// call and the raw result JSON. Harnx's default emits an
    /// `AgentEvent::Tool(Completed { .. })` via the unified AgentEvent
    /// sink; when no sink is installed it extracts user-display text
    /// (or YAML-pretty-prints the JSON), truncates to terminal
    /// dimensions, dims the text, and writes to stdout.
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

struct ApprovedToolCall {
    call: ToolCall,
    json_data: Value,
    tool_input: Value,
    tool_use_id: String,
}

pub async fn eval_tool_calls(
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
    let mut approved = Vec::new();

    for call in calls {
        if abort_signal.aborted() {
            bail!("interrupted during pre-tool phase");
        }

        let json_data = match parse_call_arguments(&call) {
            Ok(json_data) => json_data,
            Err(ToolError::Recoverable(err)) => {
                is_all_null = false;
                let error_result = json!({
                    "is_error": true,
                    "error": format!("{err:#}"),
                });
                output.push(ToolResult::new(call, error_result));
                continue;
            }
            Err(ToolError::Fatal(err)) => return Err(err),
        };

        let tool_input = call.arguments.clone();
        let tool_use_id = call.id.clone().unwrap_or_default();
        let pre_event = HookEvent::PreToolUse {
            tool_name: call.name.clone(),
            tool_input: tool_input.clone(),
            tool_use_id: tool_use_id.clone(),
        };
        let pre_outcome = tokio::select! {
            outcome = (ctx.dispatch_hook_fn)(pre_event) => outcome,
            _ = wait_abort_signal(abort_signal) => HookOutcome {
                control: HookResultControl::Block {
                    reason: "cancelled by user".to_string(),
                },
                result: HookResult::default(),
            },
        };
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
            if !(ctx.confirm_tool_use_fn)(&call.name, &json_data, reason.as_deref()) {
                let deny_reason = reason.unwrap_or_else(|| "Denied by user".to_string());
                let blocked_result = json!({"error": deny_reason, "blocked_by_hook": true});
                output.push(ToolResult::new(call, blocked_result));
                is_all_null = false;
                continue;
            }
        }

        (ctx.emit_tool_call_fn)(&call, &json_data);
        approved.push(ApprovedToolCall {
            call,
            json_data,
            tool_input,
            tool_use_id,
        });
    }

    let dispatch_futures = approved.iter().map(|approved_call| {
        let call = approved_call.call.clone();
        let json_data = approved_call.json_data.clone();
        async move {
            tokio::select! {
                result = dispatch_tool_call(call, json_data, ctx, abort_signal) => result,
                _ = wait_abort_signal(abort_signal) => Err(ToolError::Recoverable(anyhow!("aborted"))),
            }
        }
    });
    let dispatch_results = join_all(dispatch_futures).await;

    let mut fatal_err = None;
    for (approved_call, result) in approved.into_iter().zip(dispatch_results) {
        let ApprovedToolCall {
            call,
            tool_input,
            tool_use_id,
            ..
        } = approved_call;

        match result {
            Ok(mut result) => {
                let post_event = HookEvent::PostToolUse {
                    tool_name: call.name.clone(),
                    tool_input: tool_input.clone(),
                    tool_use_id: tool_use_id.clone(),
                    tool_response: result.clone(),
                };
                let _ = (ctx.dispatch_hook_fn)(post_event).await;
                (ctx.emit_tool_result_fn)(&call, &result);
                if !result.is_null() {
                    is_all_null = false;
                } else {
                    result = json!("DONE");
                }
                let mut result_obj = ToolResult::new(call, result);
                result_obj.switch_agent = detect_switch_agent(&result_obj.output);
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
                let _ = (ctx.dispatch_hook_fn)(fail_event).await;

                is_all_null = false;
                let error_result = json!({
                    "is_error": true,
                    "error": error_display,
                });
                (ctx.emit_tool_result_fn)(&call, &error_result);
                output.push(ToolResult::new(call, error_result));
            }
            Err(ToolError::Fatal(err)) => {
                let error_display = format!("{err:#}");
                (ctx.emit_tool_result_fn)(
                    &call,
                    &json!({
                        "is_error": true,
                        "error": error_display,
                    }),
                );
                if fatal_err.is_none() {
                    fatal_err = Some(err);
                }
            }
        }
    }

    if let Some(err) = fatal_err {
        return Err(err);
    }
    if is_all_null {
        output = vec![];
    }
    Ok(output)
}

fn parse_call_arguments(call: &ToolCall) -> Result<Value, ToolError> {
    if call.arguments.is_null() {
        return Ok(Value::Null);
    }
    if call.arguments.is_object() {
        return Ok(call.arguments.clone());
    }
    if let Some(arguments) = call.arguments.as_str() {
        return serde_json::from_str(arguments).map_err(|_| {
            ToolError::Recoverable(anyhow!(
                "The call '{}' has invalid arguments: {arguments}",
                call.name
            ))
        });
    }
    Err(ToolError::Recoverable(anyhow!(
        "The call '{}' has invalid arguments: {}",
        call.name,
        call.arguments
    )))
}

fn detect_switch_agent(output: &Value) -> Option<SwitchAgentData> {
    let obj = output.as_object()?;
    if obj.get("action").and_then(|v| v.as_str()) != Some("switch_agent") {
        return None;
    }
    let agent = obj.get("agent").and_then(|v| v.as_str())?;
    let prompt = obj.get("prompt").and_then(|v| v.as_str())?;
    Some(SwitchAgentData {
        agent: agent.to_string(),
        prompt: prompt.to_string(),
        session_id: obj
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(ToString::to_string),
    })
}

async fn dispatch_tool_call(
    call: ToolCall,
    json_data: Value,
    ctx: &ToolEvalContext,
    abort_signal: &AbortSignal,
) -> Result<Value, ToolError> {
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
        let result = provider
            .call_tool(&tool_name, json_data.clone(), abort_signal)
            .await?;
        return Ok(result);
    }

    Err(ToolError::Recoverable(anyhow!(
        "No tool provider configured for '{}'",
        call.name
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use harnx_core::abort::create_abort_signal;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;
    use tokio::sync::Mutex;
    use tokio::time::Instant;

    struct MockToolProvider {
        tool_name: String,
        delay: Duration,
        result: Mutex<Option<Result<Value, ToolError>>>,
        panic_on_call: bool,
    }

    impl MockToolProvider {
        fn ok(tool_name: &str, delay: Duration, result: Value) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                delay,
                result: Mutex::new(Some(Ok(result))),
                panic_on_call: false,
            }
        }

        fn err(tool_name: &str, delay: Duration, error: ToolError) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                delay,
                result: Mutex::new(Some(Err(error))),
                panic_on_call: false,
            }
        }

        fn panic(tool_name: &str) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                delay: Duration::ZERO,
                result: Mutex::new(None),
                panic_on_call: true,
            }
        }
    }

    impl ToolProvider for MockToolProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn has_tool(&self, tool_name: &str) -> bool {
            self.tool_name == tool_name
        }

        fn call_tool<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            tool_name: &'life1 str,
            _arguments: Value,
            _abort: &'life2 AbortSignal,
        ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                assert_eq!(tool_name, self.tool_name);
                assert!(!self.panic_on_call, "tool should not have been dispatched");
                tokio::time::sleep(self.delay).await;
                self.result
                    .lock()
                    .await
                    .take()
                    .expect("mock tool called more than once")
            })
        }
    }

    fn continue_hook_outcome() -> HookOutcome {
        HookOutcome {
            control: HookResultControl::Continue,
            result: HookResult::default(),
        }
    }

    fn test_context(
        providers: Vec<Arc<dyn ToolProvider>>,
        dispatch_hook: impl Fn(HookEvent) -> HookOutcome + Send + Sync + 'static,
    ) -> ToolEvalContext {
        test_context_with_emitters(providers, dispatch_hook, |_, _| {}, |_, _| {})
    }

    fn test_context_with_emitters(
        providers: Vec<Arc<dyn ToolProvider>>,
        dispatch_hook: impl Fn(HookEvent) -> HookOutcome + Send + Sync + 'static,
        emit_tool_call: impl Fn(&ToolCall, &Value) + Send + Sync + 'static,
        emit_tool_result: impl Fn(&ToolCall, &Value) + Send + Sync + 'static,
    ) -> ToolEvalContext {
        ToolEvalContext {
            providers,
            session_name: None,
            allowed_tool_names: HashSet::new(),
            emit_tool_call_fn: Arc::new(emit_tool_call),
            emit_tool_result_fn: Arc::new(emit_tool_result),
            confirm_tool_use_fn: Arc::new(|_, _, _| true),
            dispatch_hook_fn: Arc::new(move |event| {
                let outcome = dispatch_hook(event);
                Box::pin(async move { outcome })
            }),
        }
    }

    fn two_tool_context(
        name_a: &str,
        delay_a: Duration,
        result_a: Value,
        name_b: &str,
        delay_b: Duration,
        result_b: Value,
    ) -> ToolEvalContext {
        test_context(
            vec![
                Arc::new(MockToolProvider::ok(name_a, delay_a, result_a)),
                Arc::new(MockToolProvider::ok(name_b, delay_b, result_b)),
            ],
            |_| continue_hook_outcome(),
        )
    }

    fn test_call(name: &str) -> ToolCall {
        ToolCall::new(name.to_string(), json!({}), None, None)
    }

    #[tokio::test]
    async fn parallel_calls_run_concurrently() {
        let ctx = two_tool_context(
            "tool_a",
            Duration::from_millis(50),
            json!("a"),
            "tool_b",
            Duration::from_millis(50),
            json!("b"),
        );
        let abort_signal = create_abort_signal();

        let start = Instant::now();
        let result = eval_tool_calls(
            &ctx,
            vec![test_call("tool_a"), test_call("tool_b")],
            &abort_signal,
        )
        .await
        .expect("tool calls should succeed");
        let elapsed = start.elapsed();

        assert_eq!(result.len(), 2);
        assert!(elapsed < Duration::from_millis(80), "elapsed: {elapsed:?}");
    }

    #[tokio::test]
    async fn result_order_preserved() {
        let ctx = two_tool_context(
            "tool_a",
            Duration::from_millis(60),
            json!("slow"),
            "tool_b",
            Duration::from_millis(10),
            json!("fast"),
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(
            &ctx,
            vec![test_call("tool_a"), test_call("tool_b")],
            &abort_signal,
        )
        .await
        .expect("tool calls should succeed");

        assert_eq!(result[0].output, json!("slow"));
        assert_eq!(result[1].output, json!("fast"));
    }

    #[tokio::test]
    async fn fatal_error_propagates() {
        let ctx = test_context(
            vec![Arc::new(MockToolProvider::err(
                "tool_a",
                Duration::ZERO,
                ToolError::Fatal(anyhow!("boom")),
            ))],
            |_| continue_hook_outcome(),
        );
        let abort_signal = create_abort_signal();

        let err = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect_err("fatal error should propagate");

        assert!(format!("{err:#}").contains("boom"));
    }

    #[tokio::test]
    async fn blocked_call_not_dispatched() {
        let ctx =
            test_context(
                vec![Arc::new(MockToolProvider::panic("tool_a"))],
                |event| match event {
                    HookEvent::PreToolUse { .. } => HookOutcome {
                        control: HookResultControl::Block {
                            reason: "no".to_string(),
                        },
                        result: HookResult::default(),
                    },
                    _ => continue_hook_outcome(),
                },
            );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("blocked call should still return output");

        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].output,
            json!({"error": "no", "blocked_by_hook": true})
        );
    }

    #[tokio::test]
    async fn recoverable_error_emits_result() {
        let result_emit_count = Arc::new(AtomicUsize::new(0));
        let result_emit_count_clone = Arc::clone(&result_emit_count);
        let ctx = test_context_with_emitters(
            vec![Arc::new(MockToolProvider::err(
                "tool_a",
                Duration::ZERO,
                ToolError::Recoverable(anyhow!("retry")),
            ))],
            |_| continue_hook_outcome(),
            |_, _| {},
            move |_, _| {
                result_emit_count_clone.fetch_add(1, Ordering::SeqCst);
            },
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("recoverable error should return output");

        assert_eq!(result.len(), 1);
        assert_eq!(result_emit_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn blocked_call_does_not_emit_started() {
        let started_emit_count = Arc::new(AtomicUsize::new(0));
        let started_emit_count_clone = Arc::clone(&started_emit_count);
        let ctx = test_context_with_emitters(
            vec![Arc::new(MockToolProvider::panic("tool_a"))],
            |event| match event {
                HookEvent::PreToolUse { .. } => HookOutcome {
                    control: HookResultControl::Block {
                        reason: "no".to_string(),
                    },
                    result: HookResult::default(),
                },
                _ => continue_hook_outcome(),
            },
            move |_, _| {
                started_emit_count_clone.fetch_add(1, Ordering::SeqCst);
            },
            |_, _| {},
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("blocked call should still return output");

        assert_eq!(result.len(), 1);
        assert_eq!(started_emit_count.load(Ordering::SeqCst), 0);
    }
}
