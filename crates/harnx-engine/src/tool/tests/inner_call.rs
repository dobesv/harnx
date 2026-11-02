use super::*;
use harnx_core::partial_result::error_with_partial_result;
use std::io::{Error as IoError, ErrorKind};

// The engine takes a hook callback rather than a HookProvider trait.
type RecordedHooks = Arc<std::sync::Mutex<Vec<HookEvent>>>;

fn recording_context(
    providers: Vec<Arc<dyn ToolProvider>>,
    pre_outcome: HookOutcome,
) -> (ToolEvalContext, RecordedHooks) {
    let events = RecordedHooks::default();
    let captured = Arc::clone(&events);
    let ctx = test_context(providers, move |event| {
        let outcome = if matches!(&event, HookEvent::PreToolUse { .. }) {
            pre_outcome.clone()
        } else {
            continue_hook_outcome()
        };
        captured.lock().unwrap().push(event);
        outcome
    });
    (ctx, events)
}

fn assert_hook_identity(call: &ToolCall, pre_id: &str, post_id: &str) {
    assert!(!pre_id.is_empty());
    assert_eq!(post_id, pre_id);
    assert_eq!(call.id.as_deref(), Some(pre_id));
}

fn assert_hook_inputs(pre: (&str, &Value), post: (&str, &Value)) {
    assert_eq!(pre.0, "tool_a");
    assert_eq!(pre.1, &json!({}));
    assert_eq!(post, pre);
}

fn assert_success_hooks(hooks: &[HookEvent], result: &ToolResult, output: &Value) {
    let [HookEvent::PreToolUse {
        tool_name: pre_name,
        tool_input: pre_input,
        tool_use_id: pre_id,
    }, HookEvent::PostToolUse {
        tool_name: post_name,
        tool_input: post_input,
        tool_use_id: post_id,
        tool_response,
    }] = hooks
    else {
        panic!("expected exactly one pre and one post hook: {hooks:?}");
    };
    assert_hook_inputs((pre_name, pre_input), (post_name, post_input));
    assert_hook_identity(&result.call, pre_id, post_id);
    assert_eq!(tool_response, output);
}

fn assert_deferred_approval(approval: &ToolApprovalRequiredError) {
    assert_eq!(approval.tool_calls().len(), 1);
    assert_eq!(approval.tool_calls()[0].id.as_deref(), Some("inner-call"));
    let [deferred] = approval.deferred_calls() else {
        panic!("expected exactly one deferred call");
    };
    assert_eq!(
        (
            deferred.call.id.as_deref(),
            &deferred.arguments,
            deferred.reason.as_deref()
        ),
        (
            Some("inner-call"),
            &json!({"injected": true}),
            Some("approval needed")
        )
    );
}

#[tokio::test]
async fn preserves_outputs_and_runs_pre_and_post_hooks_once() {
    for output in [
        Value::Null,
        json!({"ok": true}),
        json!({"isError": true, "content": [{"type": "text", "text": "tool failed"}]}),
        json!({"content": [{"type": "image", "mimeType": "image/png", "data": "Zm9v"}]}),
    ] {
        let (mut ctx, hooks) = recording_context(
            vec![Arc::new(MockToolProvider::ok(
                "tool_a",
                Duration::ZERO,
                output.clone(),
            ))],
            continue_hook_outcome(),
        );
        ctx.confirm_tool_use_fn = Arc::new(|_, _, _| panic!("Continue needs no confirmation"));
        let result = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
            .await
            .unwrap();

        assert_eq!(result.output, output);
        assert!(
            result.content.is_empty(),
            "lossless calls leave media inline"
        );
        assert_success_hooks(&hooks.lock().unwrap(), &result, &output);
    }
}

#[tokio::test]
async fn defer_returns_typed_approval_error_without_dispatch_or_post_hooks() {
    let (mut ctx, hooks) = recording_context(
        vec![Arc::new(MockToolProvider::panic("tool_a"))],
        HookOutcome {
            control: HookResultControl::Ask {
                reason: Some("approval needed".into()),
            },
            result: HookResult {
                mutated_tool_input: Some(json!({"injected": true})),
                ..Default::default()
            },
        },
    );
    let confirmations = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&confirmations);
    ctx.confirm_tool_use_fn = Arc::new(move |call, arguments, reason| {
        captured.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            (call.name.as_str(), call.id.as_deref()),
            ("tool_a", Some("inner-call"))
        );
        assert_eq!(arguments, &json!({"injected": true}));
        assert_eq!(reason, Some("approval needed"));
        ToolUseConfirmation::Defer
    });
    let original_confirm = Arc::clone(&ctx.confirm_tool_use_fn);
    let call = ToolCall::new("tool_a".into(), json!({}), Some("inner-call".into()), None);
    let error = eval_inner_tool_call(&ctx, call, &create_abort_signal())
        .await
        .unwrap_err();

    let approval = error.downcast_ref::<ToolApprovalRequiredError>().unwrap();
    assert_deferred_approval(approval);
    assert_eq!(confirmations.load(Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(&ctx.confirm_tool_use_fn, &original_confirm));
    assert!(matches!(
        hooks.lock().unwrap().as_slice(),
        [HookEvent::PreToolUse { .. }]
    ));
}

#[tokio::test]
async fn block_returns_structured_error_without_dispatch_or_post_hooks() {
    let (mut ctx, hooks) = recording_context(
        vec![Arc::new(MockToolProvider::panic("tool_a"))],
        HookOutcome {
            control: HookResultControl::Block {
                reason: "blocked by policy".into(),
            },
            result: HookResult::default(),
        },
    );
    ctx.confirm_tool_use_fn = Arc::new(|_, _, _| panic!("Block must not ask for approval"));
    let blocked = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&blocked);
    ctx.emit_tool_blocked_fn = Arc::new(move |_, _| {
        captured.fetch_add(1, Ordering::SeqCst);
    });
    let result = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
        .await
        .unwrap();

    assert_eq!(
        result.output,
        json!({"error": "blocked by policy", "blocked_by_hook": true})
    );
    assert_eq!(blocked.load(Ordering::SeqCst), 1);
    assert!(matches!(
        hooks.lock().unwrap().as_slice(),
        [HookEvent::PreToolUse { .. }]
    ));
}

#[tokio::test]
async fn ask_uses_context_approval_or_denial() {
    for approve in [true, false] {
        let provider = if approve {
            MockToolProvider::ok("tool_a", Duration::ZERO, Value::Null)
        } else {
            MockToolProvider::panic("tool_a")
        };
        let (mut ctx, hooks) = recording_context(
            vec![Arc::new(provider)],
            HookOutcome {
                control: HookResultControl::Ask { reason: None },
                result: HookResult::default(),
            },
        );
        let confirmations = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&confirmations);
        ctx.confirm_tool_use_fn = Arc::new(move |_, _, _| {
            captured.fetch_add(1, Ordering::SeqCst);
            if approve {
                ToolUseConfirmation::Approve
            } else {
                ToolUseConfirmation::Deny {
                    reason: Some("not approved".into()),
                }
            }
        });
        let result = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
            .await
            .unwrap();

        assert_eq!(confirmations.load(Ordering::SeqCst), 1);
        let hooks = hooks.lock().unwrap();
        if approve {
            assert_eq!(result.output, Value::Null);
            assert!(matches!(
                hooks.as_slice(),
                [HookEvent::PreToolUse { .. }, HookEvent::PostToolUse { .. }]
            ));
        } else {
            assert_eq!(
                result.output,
                json!({"error": "not approved", "blocked_by_hook": true})
            );
            assert!(matches!(hooks.as_slice(), [HookEvent::PreToolUse { .. }]));
        }
    }
}

#[tokio::test]
async fn provider_error_keeps_partial_result_and_runs_failure_hook_once() {
    let error =
        error_with_partial_result(anyhow!("child failed"), json!({"session_id": "child-1"}));
    let (ctx, hooks) = recording_context(
        vec![Arc::new(MockToolProvider::err(
            "tool_a",
            Duration::ZERO,
            ToolError::Recoverable(error),
        ))],
        continue_hook_outcome(),
    );
    let result = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
        .await
        .unwrap();

    assert_eq!(
        result.output,
        json!({
            "is_error": true,
            "error": "child failed",
            "partial_result": {"session_id": "child-1"},
        })
    );
    assert_failure_hooks(&hooks.lock().unwrap(), &result.call);
}

fn assert_failure_hooks(hooks: &[HookEvent], call: &ToolCall) {
    let [HookEvent::PreToolUse { tool_use_id, .. }, HookEvent::PostToolUseFailure {
        tool_name,
        tool_input,
        tool_use_id: failure_id,
        error,
    }] = hooks
    else {
        panic!("expected one pre and one failure hook: {hooks:?}");
    };
    assert_eq!((tool_name.as_str(), tool_input), ("tool_a", &json!({})));
    assert_hook_identity(call, tool_use_id, failure_id);
    assert_eq!(error, "child failed");
}

#[tokio::test]
async fn fatal_provider_errors_propagate_without_post_hooks() {
    for kind in [
        ErrorKind::Interrupted,
        ErrorKind::TimedOut,
        ErrorKind::PermissionDenied,
    ] {
        let error = anyhow::Error::new(IoError::from(kind)).context("provider stopped");
        let (ctx, hooks) = recording_context(
            vec![Arc::new(MockToolProvider::err(
                "tool_a",
                Duration::ZERO,
                ToolError::Fatal(error),
            ))],
            continue_hook_outcome(),
        );
        let error = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
            .await
            .unwrap_err();

        assert_eq!(error.downcast_ref::<IoError>().unwrap().kind(), kind);
        assert_eq!(error.to_string(), "provider stopped");
        assert!(matches!(
            hooks.lock().unwrap().as_slice(),
            [HookEvent::PreToolUse { .. }]
        ));
    }
}

#[tokio::test]
async fn abort_before_or_during_admission_propagates_as_error() {
    for before_admission in [true, false] {
        let abort = create_abort_signal();
        if before_admission {
            abort.set_ctrlc();
        }
        let captured_abort = Arc::clone(&abort);
        let pre_hooks = Arc::new(AtomicUsize::new(0));
        let captured_hooks = Arc::clone(&pre_hooks);
        let ctx = test_context(
            vec![Arc::new(MockToolProvider::panic("tool_a"))],
            move |event| {
                assert!(matches!(event, HookEvent::PreToolUse { .. }));
                captured_hooks.fetch_add(1, Ordering::SeqCst);
                captured_abort.set_ctrlc();
                continue_hook_outcome()
            },
        );
        let error = eval_inner_tool_call(&ctx, test_call("tool_a"), &abort)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "interrupted during tool execution");
        assert_eq!(
            pre_hooks.load(Ordering::SeqCst),
            usize::from(!before_admission)
        );
    }
}

#[tokio::test]
async fn work_boundary_errors_propagate_without_provider_dispatch() {
    for kind in [ErrorKind::TimedOut, ErrorKind::PermissionDenied] {
        let (mut ctx, hooks) = recording_context(
            vec![Arc::new(MockToolProvider::panic("tool_a"))],
            continue_hook_outcome(),
        );
        ctx.work_boundary = Some(Arc::new(move |boundary, _| {
            Box::pin(async move {
                if matches!(boundary, WorkBoundary::Start) {
                    return Err(IoError::from(kind).into());
                }
                Ok(())
            })
        }));
        let error = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
            .await
            .unwrap_err();

        assert_eq!(error.downcast_ref::<IoError>().unwrap().kind(), kind);
        assert!(matches!(
            hooks.lock().unwrap().as_slice(),
            [HookEvent::PreToolUse { .. }]
        ));
    }
}

fn synchronize_dispatch(ctx: &mut ToolEvalContext) {
    // Neither evaluation can finish before both reach provider dispatch.
    let dispatch_barrier = Arc::new(tokio::sync::Barrier::new(2));
    ctx.work_boundary = Some(Arc::new(move |boundary, _| {
        let barrier = Arc::clone(&dispatch_barrier);
        Box::pin(async move {
            if matches!(boundary, WorkBoundary::Start) {
                barrier.wait().await;
            }
            Ok(())
        })
    }));
}

fn assert_call_hook_counts(hooks: &[HookEvent], id: &str) {
    assert_eq!(
        hooks
            .iter()
            .filter(|event| matches!(event,
                HookEvent::PreToolUse { tool_use_id, .. } if tool_use_id == id
            ))
            .count(),
        1
    );
    assert_eq!(
        hooks
            .iter()
            .filter(|event| matches!(event,
                HookEvent::PostToolUse { tool_use_id, .. } if tool_use_id == id
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn concurrent_calls_share_context_and_route_progress_by_id() {
    let provider = Arc::new(UpdatingToolProvider::default());
    let origins = Arc::clone(&provider.origins);
    let (mut ctx, hooks) = recording_context(vec![provider], continue_hook_outcome());
    ctx.tool_round = Some(12);
    synchronize_dispatch(&mut ctx);
    let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&updates);
    ctx.emit_tool_update_fn = Arc::new(move |id, patch| {
        captured
            .lock()
            .unwrap()
            .push((id.to_owned(), patch.clone()));
    });
    let ctx = Arc::new(ctx);
    let abort = create_abort_signal();
    let handles = (0..2).map(|_| {
        let ctx = Arc::clone(&ctx);
        let abort = Arc::clone(&abort);
        tokio::spawn(async move {
            eval_inner_tool_call(&ctx, test_call("updating_tool"), &abort)
                .await
                .unwrap()
        })
    });
    let results: Vec<_> = join_all(handles)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_ne!(results[0].call.id, results[1].call.id);
    assert_eq!(ctx.tool_round, Some(12));
    assert_eq!(
        (origins.lock().unwrap().len(), hooks.lock().unwrap().len()),
        (2, 4)
    );
    for result in results {
        let id = result.call.id.unwrap();
        assert!(origins
            .lock()
            .unwrap()
            .contains(&(Some(id.clone()), Some(12))));
        assert_call_hook_counts(&hooks.lock().unwrap(), &id);
        let updates = updates.lock().unwrap();
        let last = updates.iter().rfind(|(call_id, _)| call_id == &id).unwrap();
        assert_eq!(last.1.title.as_deref(), Some("second"));
    }
}
