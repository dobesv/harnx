use super::*;
use crate::nats_hook_provider::{
    dispatch_hook_event, DiscoveredHook, HookDispatchMeta, HookEventDispatch, NatsHookProvider,
};
use harnx_core::{
    abort::create_abort_signal,
    instance::ServerScope,
    tool::{ToolError, ToolProvider, ToolProviderOutput},
};
use harnx_engine::tool::{
    eval_tool_calls, ToolApprovalRequiredError, ToolEvalRenderContext, ToolUseConfirmation,
};
use harnx_hooks::{HookEvent, HookOutcome, HookResult, HookResultControl};
use harnx_hookset::{FailPolicy, HookSpec};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

struct Provider {
    seen: Arc<Mutex<Vec<Value>>>,
    output: Value,
    wait: bool,
}
#[async_trait::async_trait]
impl ToolProvider for Provider {
    fn name(&self) -> &str {
        "operator-fixture"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "visible" || name == "hidden"
    }
    async fn call_tool(
        &self,
        _: &str,
        args: Value,
        abort: &crate::utils::AbortSignal,
    ) -> std::result::Result<ToolProviderOutput, ToolError> {
        if self.wait {
            crate::utils::wait_abort_signal(abort).await;
            return Err(ToolError::Fatal(anyhow::anyhow!("cancelled fixture")));
        }
        if !args.get("text").is_some_and(Value::is_string) {
            return Err(ToolError::Recoverable(anyhow::anyhow!(
                "text must be a string"
            )));
        }
        self.seen.lock().unwrap().push(args);
        Ok(self.output.clone().into())
    }
}

fn outcome(control: HookResultControl) -> HookOutcome {
    HookOutcome {
        control,
        result: HookResult::default(),
    }
}

fn context(
    provider: Arc<dyn ToolProvider>,
    hook: impl Fn(HookEvent) -> HookOutcome + Send + Sync + 'static,
) -> ToolEvalContext {
    let scope = ServerScope::new();
    let hook = Arc::new(hook);
    let hooks = ["PreToolUse", "PostToolUseFailure"]
        .into_iter()
        .map(|event| DiscoveredHook {
            server: "policy".into(),
            display_label: None,
            spec: HookSpec {
                event: event.into(),
                matcher: None,
                priority: 0,
                timeout_secs: None,
                fail_policy: FailPolicy::Closed,
            },
        })
        .collect();
    let hook_provider = Arc::new(NatsHookProvider::from_request_handler(
        scope.clone(),
        hooks,
        Arc::new(move |_, payload| hook(payload.hook_event)),
    ));
    let decls: HashMap<_, _> = ["visible", "hidden"].into_iter().map(|name| {
        let mut declaration: ToolDeclaration = serde_json::from_value(json!({"name": name, "description": name,
            "parameters": {"type":"object", "properties":{"text":{"type":"string"}}, "required":["text"]}})).unwrap();
        declaration.mcp_server_name = Some("fixture".into());
        (name.into(), declaration)
    }).collect();
    ToolEvalContext {
        work_boundary: None,
        instance_id: scope,
        render: Some(ToolEvalRenderContext {
            decl_map: Arc::new(decls),
        }),
        providers: vec![provider],
        allowed_tool_names: HashSet::from(["visible".into()]),
        current_agent_package: None,
        handoff_targets: HashMap::new(),
        emit_tool_call_fn: Arc::new(|_, _| {}),
        emit_tool_result_fn: Arc::new(|_, _| {}),
        emit_tool_blocked_fn: Arc::new(|_, _| {}),
        emit_tool_update_fn: Arc::new(|_, _| {}),
        confirm_tool_use_fn: Arc::new(|_, _, _| ToolUseConfirmation::Defer),
        dispatch_hook_fn: Arc::new(move |event| {
            let provider = hook_provider.clone();
            Box::pin(async move {
                dispatch_hook_event(HookEventDispatch {
                    event,
                    provider: Some(&provider),
                    meta: HookDispatchMeta {
                        abort: None,
                        session_id: "active-session".into(),
                        cwd: Default::default(),
                        resume_count: 0,
                    },
                    pending_async_context: None,
                })
                .await
            })
        }),
    }
}

fn fixture(
    output: Value,
    hook: impl Fn(HookEvent) -> HookOutcome + Send + Sync + 'static,
) -> (ToolEvalContext, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    (
        context(
            Arc::new(Provider {
                seen: seen.clone(),
                output,
                wait: false,
            }),
            hook,
        ),
        seen,
    )
}
fn call(raw: &str) -> OperatorToolCommand {
    OperatorToolCommand::Call {
        name: "visible".into(),
        args_json: raw.into(),
    }
}

#[test]
fn parser_preserves_raw_json_remainder_and_action_subject_grammar() {
    let raw = "{\n  \"text\": \"a \\\"quoted\\\" path with spaces\", \"nested\": {\"array\": [1, null]}\n}  ";
    assert_eq!(
        parse_operator_line(&format!(" .call   tool\tvisible   {raw}")).unwrap(),
        call(raw)
    );
    assert_eq!(
        parse_operator_line(".list tools").unwrap(),
        OperatorToolCommand::List { pattern: None }
    );
    assert_eq!(
        parse_operator_line(".list tools visible*").unwrap(),
        OperatorToolCommand::List {
            pattern: Some("visible*".into())
        }
    );
    assert_eq!(
        parse_operator_line(".info tool visible").unwrap(),
        OperatorToolCommand::Info {
            name: "visible".into()
        }
    );
    for line in [
        ".info tools visible",
        ".list tool",
        ".call tools visible {}",
        ".call tool visible",
        ".call tool visible null",
        ".call tool visible []",
        ".call tool visible 1",
        ".call tool visible {broken}",
        ".info tool visible extra",
        ".list tools * extra",
    ] {
        assert!(parse_operator_line(line).is_err(), "{line}");
    }
}

#[tokio::test]
async fn list_info_call_and_patterns_never_expose_or_dispatch_hidden_tools() {
    for command in [
        OperatorToolCommand::Info {
            name: "hidden".into(),
        },
        OperatorToolCommand::Call {
            name: "hidden".into(),
            args_json: "{}".into(),
        },
    ] {
        let (ctx, seen) = fixture(json!(null), |_| outcome(HookResultControl::Continue));
        assert!(evaluate(
            ctx,
            &command,
            ToolOutputFormat::Human,
            &create_abort_signal()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("not available"));
        assert!(seen.lock().unwrap().is_empty());
    }
    for (pattern, count) in [
        (None, 1),
        (Some("*".into()), 1),
        (Some("hidden*".into()), 0),
        (Some("vis?ble".into()), 1),
    ] {
        let (ctx, seen) = fixture(json!(null), |_| outcome(HookResultControl::Continue));
        let reply = evaluate(
            ctx,
            &OperatorToolCommand::List { pattern },
            ToolOutputFormat::Json,
            &create_abort_signal(),
        )
        .await
        .unwrap();
        let tools: Vec<Value> = serde_json::from_str(&reply.output).unwrap();
        assert_eq!(tools.len(), count);
        assert!(tools.iter().all(|tool| tool["name"] == "visible"));
        assert!(seen.lock().unwrap().is_empty());
    }
    let (ctx, _) = fixture(json!(null), |_| outcome(HookResultControl::Continue));
    let reply = evaluate(
        ctx,
        &OperatorToolCommand::Info {
            name: "visible".into(),
        },
        ToolOutputFormat::Human,
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(reply.output.contains("Input schema:") && reply.output.contains("fixture"));
}

#[tokio::test]
async fn root_ask_uses_real_hook_dispatch_and_retains_input_transformation() {
    let (mut ctx, seen) = fixture(json!({"ok": true}), |event| {
        if matches!(event, HookEvent::PreToolUse { .. }) {
            HookOutcome {
                control: HookResultControl::Ask {
                    reason: Some("root consent".into()),
                },
                result: HookResult {
                    mutated_tool_input: Some(json!({"text":"transformed value"})),
                    ..Default::default()
                },
            }
        } else {
            outcome(HookResultControl::Continue)
        }
    });
    ctx.confirm_tool_use_fn = Arc::new(|_, _, _| panic!("root Ask must use scoped consent"));
    let reply = evaluate(
        ctx,
        &call(r#"{"text":"raw value"}"#),
        ToolOutputFormat::Human,
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(reply.error.is_none());
    assert_eq!(*seen.lock().unwrap(), [json!({"text":"transformed value"})]);
}

#[tokio::test]
async fn deny_and_validation_failures_remain_errors_with_full_output() {
    let (ctx, seen) = fixture(json!(null), |_| {
        outcome(HookResultControl::Block {
            reason: "policy deny".into(),
        })
    });
    let reply = evaluate(
        ctx,
        &call(r#"{"text":"ok"}"#),
        ToolOutputFormat::Json,
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert_eq!(reply.error.as_deref(), Some("policy deny"));
    assert_eq!(
        serde_json::from_str::<Value>(&reply.output).unwrap(),
        json!({"error":"policy deny", "blocked_by_hook":true})
    );
    assert!(seen.lock().unwrap().is_empty());
    let failures = Arc::new(AtomicUsize::new(0));
    let captured = failures.clone();
    let (ctx, seen) = fixture(json!(null), move |event| {
        if matches!(event, HookEvent::PostToolUseFailure { .. }) {
            captured.fetch_add(1, Ordering::SeqCst);
        }
        outcome(HookResultControl::Continue)
    });
    let reply = evaluate(
        ctx,
        &call(r#"{"text":123}"#),
        ToolOutputFormat::Json,
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(reply.error.is_some(), "{reply:?}");
    assert!(reply.output.contains("text must be a string"));
    assert!(seen.lock().unwrap().is_empty());
    // PostToolUseFailure dispatch is intentionally detached by the hook provider.
    for _ in 0..20 {
        if failures.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(failures.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mixed_structured_error_partial_and_null_results_are_not_reduced() {
    for output in [
        json!(null),
        json!({"content":[{"type":"text","text":"quoted \"name\""}, {"type":"image","data":"aW1hZ2U=","mimeType":"image/png"}],
        "structuredContent":{"items":[1,null]}, "_meta":{"keep":true}}),
        json!({"isError":true,"content":[{"type":"text","text":"failed"}]}),
        json!({"resultType":"partial","content":[{"type":"text","text":"unfinished"}]}),
    ] {
        let (ctx, _) = fixture(output.clone(), |_| outcome(HookResultControl::Continue));
        let reply = evaluate(
            ctx,
            &call(r#"{"text":"ok"}"#),
            ToolOutputFormat::Json,
            &create_abort_signal(),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&reply.output).unwrap(),
            output
        );
        assert_eq!(reply.error.is_some(), result_error(&output).is_some());
    }
}

#[tokio::test]
async fn cancellation_during_provider_and_before_dispatch_is_not_success() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ctx = context(
        Arc::new(Provider {
            seen: seen.clone(),
            output: json!(true),
            wait: true,
        }),
        |_| outcome(HookResultControl::Continue),
    );
    let abort = create_abort_signal();
    let signal = abort.clone();
    let cancel = tokio::spawn(async move {
        tokio::task::yield_now().await;
        signal.set_ctrlc();
    });
    assert!(evaluate(
        ctx,
        &call(r#"{"text":"ok"}"#),
        ToolOutputFormat::Json,
        &abort
    )
    .await
    .is_err());
    cancel.await.unwrap();
    assert!(seen.lock().unwrap().is_empty());
    let (ctx, seen) = fixture(json!(null), |_| outcome(HookResultControl::Continue));
    assert!(evaluate(ctx, &call("{}"), ToolOutputFormat::Human, &abort)
        .await
        .is_err());
    assert!(seen.lock().unwrap().is_empty());
}

struct NestedProvider {
    nested: ToolEvalContext,
    approvals: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl ToolProvider for NestedProvider {
    fn name(&self) -> &str {
        "nested-agent-fixture"
    }
    fn has_tool(&self, name: &str) -> bool {
        name == "visible"
    }
    async fn call_tool(
        &self,
        _: &str,
        _: Value,
        abort: &crate::utils::AbortSignal,
    ) -> std::result::Result<ToolProviderOutput, ToolError> {
        let error = eval_tool_calls(
            &self.nested,
            vec![ToolCall::new(
                "visible".into(),
                json!({"text":"child"}),
                None,
                None,
            )],
            abort,
        )
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<ToolApprovalRequiredError>().is_some());
        assert_eq!(self.approvals.load(Ordering::SeqCst), 1);
        Ok(json!({"nested":"deferred"}).into())
    }
}

#[tokio::test]
async fn root_operator_consent_never_changes_nested_evaluation_confirmation() {
    let (mut nested, seen) = fixture(json!(null), |_| {
        outcome(HookResultControl::Ask {
            reason: Some("nested Ask".into()),
        })
    });
    let approvals = Arc::new(AtomicUsize::new(0));
    let captured = approvals.clone();
    nested.confirm_tool_use_fn = Arc::new(move |_, _, _| {
        captured.fetch_add(1, Ordering::SeqCst);
        ToolUseConfirmation::Defer
    });
    let root = context(
        Arc::new(NestedProvider {
            nested,
            approvals: approvals.clone(),
        }),
        |_| outcome(HookResultControl::Ask { reason: None }),
    );
    let reply = evaluate(
        root,
        &call(r#"{"text":"root"}"#),
        ToolOutputFormat::Json,
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(reply.error.is_none());
    assert_eq!(
        serde_json::from_str::<Value>(&reply.output).unwrap(),
        json!({"nested":"deferred"})
    );
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn handlers_reject_missing_session_and_invalid_json_before_discovery() {
    let config = Arc::new(crate::config::ConfigLock::new(
        crate::config::Config::default(),
    ));
    for line in [
        ".info tool visible",
        ".list tools",
        r#".call tool visible {"text":"a b"}"#,
    ] {
        let mut output = Vec::new();
        let error = crate::commands::run_command_with_output(
            &config,
            create_abort_signal(),
            line,
            &mut output,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "Tool commands require an active session");
        assert!(output.is_empty());
    }
    let mut output = Vec::new();
    let error = crate::commands::run_command_with_output(
        &config,
        create_abort_signal(),
        ".call tool visible []",
        &mut output,
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "Tool arguments must be a JSON object");
}
