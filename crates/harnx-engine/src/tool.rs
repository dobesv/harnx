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
use std::collections::{HashMap, HashSet};
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
    /// provider but are handled directly in `dispatch_tool_call`
    /// (currently `_session_handoff`).
    pub allowed_tool_names: HashSet<String>,
    /// Package of the currently active agent (e.g. `Some("pantheon")` for
    /// `pantheon/daedalus`, `None` for a top-level agent). Used to resolve
    /// bare `_session_handoff` targets relative to the current package so a
    /// same-package handoff (`atlas_session_handoff`) lands on
    /// `pantheon/atlas` rather than top-level `atlas` (#709).
    pub current_agent_package: Option<String>,
    /// Exact decode table for package-aware handoff display names.
    /// Maps tool display name without `_session_handoff` suffix to
    /// qualified-or-bare target agent name.
    pub handoff_targets: HashMap<String, String>,
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
    /// Called when a PreToolUse hook blocks or a user denies a tool call.
    /// Receives the tool call and the blocked result JSON (contains
    /// `"error"` key). Harnx's default emits
    /// `AgentEvent::Tool(ToolEvent::Blocked { .. })`.
    pub emit_tool_blocked_fn: Arc<ToolCallEmitFn>,
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
            (ctx.emit_tool_blocked_fn)(&call, &blocked_result);
            output.push(ToolResult::new(call, blocked_result));
            is_all_null = false;
            continue;
        }
        let (json_data, tool_input) = if let Some(mutated) = pre_outcome.result.mutated_tool_input {
            (mutated.clone(), mutated)
        } else {
            (json_data, tool_input)
        };

        if let HookResultControl::Ask { reason } = pre_outcome.control {
            if !(ctx.confirm_tool_use_fn)(&call.name, &json_data, reason.as_deref()) {
                let deny_reason = reason.unwrap_or_else(|| "Denied by user".to_string());
                let blocked_result = json!({"error": deny_reason, "blocked_by_hook": true});
                (ctx.emit_tool_blocked_fn)(&call, &blocked_result);
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
        async move { dispatch_tool_call(call, json_data, ctx, abort_signal).await }
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
                let post_outcome = (ctx.dispatch_hook_fn)(post_event).await;
                if let Some(mutated_response) = post_outcome.result.mutated_tool_response {
                    result = mutated_response;
                }
                let images = crate::media::extract_image_parts(&result);
                if !images.is_empty() {
                    crate::media::redact_image_data(&mut result);
                }
                (ctx.emit_tool_result_fn)(&call, &result);
                if !result.is_null() {
                    is_all_null = false;
                } else {
                    result = json!("DONE");
                }
                let mut result_obj = ToolResult::new(call, result);
                result_obj.content = images;
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
                output.push(ToolResult::new(call, error_result));
            }
            Err(ToolError::Fatal(err)) => {
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
        // Strip exactly one `_session_handoff` suffix (not all repeats, which
        // `trim_end_matches` would do) so an agent named `*_session_handoff`
        // resolves correctly.
        let bare_target = call
            .name
            .strip_suffix("_session_handoff")
            .unwrap_or(&call.name);
        // Resolve package-aware display names through the exact lookup table
        // first. Map values are already the canonical agent name ("pkg/stem"
        // for package agents, bare "stem" for top-level agents), so they are
        // used verbatim — re-resolving a bare top-level value against the
        // current package would wrongly qualify it (e.g. `global` →
        // `pantheon/global`). Only the legacy fallback path (no map entry,
        // e.g. test/no-context contexts) applies package-relative resolution.
        let agent = match ctx.handoff_targets.get(bare_target) {
            Some(mapped) => mapped.clone(),
            None => harnx_core::package_namespace::resolve_package_relative_name(
                bare_target,
                ctx.current_agent_package.as_deref(),
            ),
        };
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

    struct CapturingToolProvider {
        tool_name: String,
        received_arguments: Arc<Mutex<Vec<Value>>>,
        result: Value,
    }

    impl CapturingToolProvider {
        fn new(tool_name: &str, received_arguments: Arc<Mutex<Vec<Value>>>, result: Value) -> Self {
            Self {
                tool_name: tool_name.to_string(),
                received_arguments,
                result,
            }
        }
    }

    impl ToolProvider for CapturingToolProvider {
        fn name(&self) -> &str {
            "capturing-mock"
        }

        fn has_tool(&self, tool_name: &str) -> bool {
            self.tool_name == tool_name
        }

        fn call_tool<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            tool_name: &'life1 str,
            arguments: Value,
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
                self.received_arguments.lock().await.push(arguments);
                Ok(self.result.clone())
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
        test_context_with_emitters(providers, dispatch_hook, |_, _| {}, |_, _| {}, |_, _| {})
    }

    fn test_context_with_emitters(
        providers: Vec<Arc<dyn ToolProvider>>,
        dispatch_hook: impl Fn(HookEvent) -> HookOutcome + Send + Sync + 'static,
        emit_tool_call: impl Fn(&ToolCall, &Value) + Send + Sync + 'static,
        emit_tool_result: impl Fn(&ToolCall, &Value) + Send + Sync + 'static,
        emit_tool_blocked: impl Fn(&ToolCall, &Value) + Send + Sync + 'static,
    ) -> ToolEvalContext {
        ToolEvalContext {
            providers,
            session_name: None,
            allowed_tool_names: HashSet::new(),
            current_agent_package: None,
            handoff_targets: HashMap::new(),
            emit_tool_call_fn: Arc::new(emit_tool_call),
            emit_tool_result_fn: Arc::new(emit_tool_result),
            emit_tool_blocked_fn: Arc::new(emit_tool_blocked),
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
    async fn recoverable_error_does_not_emit_result() {
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
            |_, _| {},
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("recoverable error should return output");

        assert_eq!(result.len(), 1);
        assert_eq!(result_emit_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn blocked_call_emits_blocked_event() {
        let started_emit_count = Arc::new(AtomicUsize::new(0));
        let started_emit_count_clone = Arc::clone(&started_emit_count);
        let blocked_emit_count = Arc::new(AtomicUsize::new(0));
        let blocked_emit_count_clone = Arc::clone(&blocked_emit_count);
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
            move |_, _| {
                blocked_emit_count_clone.fetch_add(1, Ordering::SeqCst);
            },
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("blocked call should still return output");

        assert_eq!(result.len(), 1);
        assert_eq!(started_emit_count.load(Ordering::SeqCst), 0);
        assert_eq!(blocked_emit_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pre_tool_use_mutation_applied_to_tool() {
        let received_arguments = Arc::new(Mutex::new(Vec::new()));
        let received_arguments_clone = Arc::clone(&received_arguments);
        let ctx = test_context_with_emitters(
            vec![Arc::new(CapturingToolProvider::new(
                "tool_a",
                received_arguments_clone,
                json!({"raw": true}),
            ))],
            |event| match event {
                HookEvent::PreToolUse { .. } => HookOutcome {
                    control: HookResultControl::Continue,
                    result: HookResult {
                        mutated_tool_input: Some(json!({"injected": true})),
                        ..HookResult::default()
                    },
                },
                _ => continue_hook_outcome(),
            },
            |_, _| {},
            |_, _| {},
            |_, _| {},
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(
            &ctx,
            vec![ToolCall::new(
                "tool_a".to_string(),
                json!({"original": true}),
                None,
                None,
            )],
            &abort_signal,
        )
        .await
        .expect("tool call should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(
            received_arguments.lock().await.as_slice(),
            &[json!({"injected": true})]
        );
    }

    #[tokio::test]
    async fn post_tool_use_mutation_replaces_result() {
        let ctx = test_context_with_emitters(
            vec![Arc::new(MockToolProvider::ok(
                "tool_a",
                Duration::ZERO,
                json!({"raw": true}),
            ))],
            |event| match event {
                HookEvent::PostToolUse { .. } => HookOutcome {
                    control: HookResultControl::Continue,
                    result: HookResult {
                        mutated_tool_response: Some(json!({"mutated": true})),
                        ..HookResult::default()
                    },
                },
                _ => continue_hook_outcome(),
            },
            |_, _| {},
            |_, _| {},
            |_, _| {},
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("tool call should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].output, json!({"mutated": true}));
    }

    #[tokio::test]
    async fn successful_tool_result_populates_image_content_parts() {
        let image_data = "Zm9v";
        let emitted_results = Arc::new(std::sync::Mutex::new(Vec::new()));
        let emitted_results_clone = Arc::clone(&emitted_results);
        let ctx = test_context_with_emitters(
            vec![Arc::new(MockToolProvider::ok(
                "tool_a",
                Duration::ZERO,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": "caption"
                        },
                        {
                            "type": "image",
                            "mimeType": "image/png",
                            "data": image_data
                        }
                    ]
                }),
            ))],
            |_| continue_hook_outcome(),
            |_, _| {},
            move |_, value| {
                emitted_results_clone
                    .lock()
                    .expect("lock emitted results")
                    .push(value.clone());
            },
            |_, _| {},
        );
        let abort_signal = create_abort_signal();

        let result = eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("tool call should succeed");

        let emitted_results = emitted_results.lock().expect("lock emitted results");
        assert_eq!(emitted_results.len(), 1);
        let emitted_output = emitted_results[0].to_string();
        assert!(!emitted_output.contains(image_data));
        assert!(emitted_output.contains("caption"));
        assert!(emitted_output.contains("<image: image/png, 4 base64 chars>"));

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content.len(), 1);
        match &result[0].content[0] {
            harnx_core::message::MessageContentPart::ImageUrl { image_url } => {
                assert_eq!(image_url.url, format!("data:image/png;base64,{image_data}"));
            }
            other => panic!("expected image content part, got {other:?}"),
        }

        let serialized_output = result[0].output.to_string();
        assert!(!serialized_output.contains(image_data));
        assert!(serialized_output.contains("caption"));
        assert!(serialized_output.contains("<image: image/png, 4 base64 chars>"));
    }

    #[tokio::test]
    async fn pre_mutation_reflected_in_post_tool_use_event() {
        let post_tool_inputs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let post_tool_inputs_clone = Arc::clone(&post_tool_inputs);
        let ctx = test_context_with_emitters(
            vec![Arc::new(MockToolProvider::ok(
                "tool_a",
                Duration::ZERO,
                json!({"raw": true}),
            ))],
            move |event| match event {
                HookEvent::PreToolUse { .. } => HookOutcome {
                    control: HookResultControl::Continue,
                    result: HookResult {
                        mutated_tool_input: Some(json!({"injected": true})),
                        ..HookResult::default()
                    },
                },
                HookEvent::PostToolUse { tool_input, .. } => {
                    post_tool_inputs_clone
                        .lock()
                        .expect("lock post tool inputs")
                        .push(tool_input);
                    continue_hook_outcome()
                }
                _ => continue_hook_outcome(),
            },
            |_, _| {},
            |_, _| {},
            |_, _| {},
        );
        let abort_signal = create_abort_signal();

        eval_tool_calls(&ctx, vec![test_call("tool_a")], &abort_signal)
            .await
            .expect("tool call should succeed");

        assert_eq!(
            post_tool_inputs
                .lock()
                .expect("lock post tool inputs")
                .as_slice(),
            &[json!({"injected": true})]
        );
    }

    /// Build a minimal context for handoff-resolution tests with a given
    /// active-agent package, allow-listed handoff tool name, and optional
    /// package-aware decode map.
    fn handoff_context(
        package: Option<&str>,
        allowed: &str,
        handoff_targets: HashMap<String, String>,
    ) -> ToolEvalContext {
        let mut ctx = test_context(vec![], |_| continue_hook_outcome());
        ctx.current_agent_package = package.map(str::to_string);
        ctx.allowed_tool_names = HashSet::from([allowed.to_string()]);
        ctx.handoff_targets = handoff_targets;
        ctx
    }

    async fn dispatched_handoff_agent(ctx: &ToolEvalContext, tool_name: &str) -> String {
        let call = ToolCall::new(tool_name.to_string(), json!({ "prompt": "go" }), None, None);
        let abort_signal = create_abort_signal();
        let result =
            match dispatch_tool_call(call, json!({ "prompt": "go" }), ctx, &abort_signal).await {
                Ok(result) => result,
                Err(ToolError::Recoverable(err) | ToolError::Fatal(err)) => {
                    panic!("handoff dispatch should succeed: {err:#}")
                }
            };
        result["agent"]
            .as_str()
            .expect("handoff result should carry an agent")
            .to_string()
    }

    #[tokio::test]
    async fn handoff_bare_target_resolves_to_current_package() {
        // #709: `pantheon/daedalus` handing off to bare `atlas` must land on
        // same-package `pantheon/atlas`, not top-level `atlas`.
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let agent = dispatched_handoff_agent(&ctx, "atlas_session_handoff").await;
        assert_eq!(agent, "pantheon/atlas");
    }

    #[tokio::test]
    async fn handoff_bare_target_top_level_unchanged() {
        // No package context (top-level agent) → bare target stays bare.
        let ctx = handoff_context(None, "atlas_session_handoff", HashMap::new());
        let agent = dispatched_handoff_agent(&ctx, "atlas_session_handoff").await;
        assert_eq!(agent, "atlas");
    }

    #[tokio::test]
    async fn handoff_leading_slash_escapes_to_top_level() {
        // `/atlas` explicitly escapes the package to top-level even when a
        // package context is present.
        let ctx = handoff_context(Some("pantheon"), "/atlas_session_handoff", HashMap::new());
        let agent = dispatched_handoff_agent(&ctx, "/atlas_session_handoff").await;
        assert_eq!(agent, "atlas");
    }

    #[tokio::test]
    async fn handoff_qualified_target_unchanged() {
        // Cross-package qualified target is left untouched.
        let ctx = handoff_context(
            Some("pantheon"),
            "other/atlas_session_handoff",
            HashMap::new(),
        );
        let agent = dispatched_handoff_agent(&ctx, "other/atlas_session_handoff").await;
        assert_eq!(agent, "other/atlas");
    }

    #[tokio::test]
    async fn handoff_uses_exact_map_for_same_package_display_name() {
        let ctx = handoff_context(
            Some("pantheon"),
            "atlas_session_handoff",
            HashMap::from([("atlas".to_string(), "pantheon/atlas".to_string())]),
        );
        let agent = dispatched_handoff_agent(&ctx, "atlas_session_handoff").await;
        assert_eq!(agent, "pantheon/atlas");
    }

    #[tokio::test]
    async fn handoff_uses_exact_map_for_cross_package_display_name() {
        let ctx = handoff_context(
            Some("pantheon"),
            "otherpkg__helper_session_handoff",
            HashMap::from([(
                "otherpkg__helper".to_string(),
                "otherpkg/helper".to_string(),
            )]),
        );
        let agent = dispatched_handoff_agent(&ctx, "otherpkg__helper_session_handoff").await;
        assert_eq!(agent, "otherpkg/helper");
    }

    #[tokio::test]
    async fn handoff_uses_exact_map_for_top_level_from_package() {
        // A top-level agent targeted from within a package is spelled `__stem`
        // and maps to the BARE top-level name. It must NOT be re-qualified into
        // the active package (would be `pantheon/global` — wrong).
        let ctx = handoff_context(
            Some("pantheon"),
            "__global_session_handoff",
            HashMap::from([("__global".to_string(), "global".to_string())]),
        );
        let agent = dispatched_handoff_agent(&ctx, "__global_session_handoff").await;
        assert_eq!(agent, "global");
    }

    #[tokio::test]
    async fn handoff_legacy_fallback_still_resolves_bare_target() {
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let agent = dispatched_handoff_agent(&ctx, "atlas_session_handoff").await;
        assert_eq!(agent, "pantheon/atlas");
    }
    #[tokio::test]
    async fn handoff_strips_only_one_suffix() {
        // An agent literally named `worker_session_handoff` must keep its name:
        // only the trailing `_session_handoff` tool suffix is stripped, not the
        // repeated occurrence in the agent name.
        let ctx = handoff_context(
            None,
            "worker_session_handoff_session_handoff",
            HashMap::new(),
        );
        let agent = dispatched_handoff_agent(&ctx, "worker_session_handoff_session_handoff").await;
        assert_eq!(agent, "worker_session_handoff");
    }

    #[tokio::test]
    async fn handoff_propagates_prompt() {
        // The supplied prompt must flow through to the switch_agent result.
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let call = ToolCall::new(
            "atlas_session_handoff".to_string(),
            json!({ "prompt": "do the thing" }),
            None,
            None,
        );
        let abort_signal = create_abort_signal();
        let result = match dispatch_tool_call(
            call,
            json!({ "prompt": "do the thing" }),
            &ctx,
            &abort_signal,
        )
        .await
        {
            Ok(result) => result,
            Err(ToolError::Recoverable(err) | ToolError::Fatal(err)) => {
                panic!("handoff dispatch should succeed: {err:#}")
            }
        };
        assert_eq!(result["action"].as_str(), Some("switch_agent"));
        assert_eq!(result["agent"].as_str(), Some("pantheon/atlas"));
        assert_eq!(result["prompt"].as_str(), Some("do the thing"));
    }

    #[tokio::test]
    async fn handoff_missing_prompt_is_recoverable_error() {
        // A handoff call without the required `prompt` argument must return a
        // Recoverable error (so the LLM can retry), not panic or switch.
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let call = ToolCall::new("atlas_session_handoff".to_string(), json!({}), None, None);
        let abort_signal = create_abort_signal();
        match dispatch_tool_call(call, json!({}), &ctx, &abort_signal).await {
            Ok(_) => panic!("missing prompt must error"),
            Err(ToolError::Recoverable(e)) => {
                assert!(e.to_string().contains("prompt"), "unexpected error: {e:#}");
            }
            Err(ToolError::Fatal(e)) => panic!("expected Recoverable, got Fatal: {e:#}"),
        }
    }

    #[tokio::test]
    async fn handoff_unallowed_tool_is_recoverable_error() {
        // A handoff tool not present in `allowed_tool_names` must be rejected
        // (no provider configured) rather than silently switching agents.
        let ctx = handoff_context(Some("pantheon"), "atlas_session_handoff", HashMap::new());
        let call = ToolCall::new(
            "unlisted_session_handoff".to_string(),
            json!({ "prompt": "go" }),
            None,
            None,
        );
        let abort_signal = create_abort_signal();
        match dispatch_tool_call(call, json!({ "prompt": "go" }), &ctx, &abort_signal).await {
            Ok(_) => panic!("unallowed handoff tool must error"),
            Err(ToolError::Recoverable(e)) => {
                assert!(
                    e.to_string().contains("unlisted_session_handoff"),
                    "unexpected error: {e:#}"
                );
            }
            Err(ToolError::Fatal(e)) => panic!("expected Recoverable, got Fatal: {e:#}"),
        }
    }
}
