use super::*;

fn silent_options(
    providers: Vec<Arc<dyn ToolProvider>>,
    tool_round: Option<u64>,
) -> InnerToolEvalOptions {
    InnerToolEvalOptions {
        providers,
        tool_round,
        emit_tool_call_fn: Arc::new(|_, _| {}),
        emit_tool_result_fn: Arc::new(|_, _| {}),
        emit_tool_blocked_fn: Arc::new(|_, _| {}),
        emit_tool_update_fn: Arc::new(|_, _| {}),
    }
}

fn context_with_metadata() -> ToolEvalContext {
    let mut outer = test_context(
        vec![
            Arc::new(MockToolProvider::panic("base_tool")),
            Arc::new(MockToolProvider::panic("bash")),
        ],
        |_| continue_hook_outcome(),
    );
    outer.tool_round = Some(1);
    outer.render = Some(ToolEvalRenderContext {
        decl_map: Arc::new(HashMap::new()),
    });
    outer.current_agent_package = Some("pantheon".into());
    outer
        .allowed_tool_names
        .insert("atlas_session_handoff".into());
    outer
        .handoff_targets
        .insert("atlas".into(), "pantheon/atlas".into());
    outer.work_boundary = Some(Arc::new(|_, _| Box::pin(async { Ok(()) })));
    outer.prepare_handoff_fn = Some(Arc::new(|_, _, _| {
        Box::pin(async { panic!("forking must not prepare a handoff") })
    }));
    outer
}

fn assert_inner_dependencies(inner: &ToolEvalContext, outer: &ToolEvalContext) {
    assert_eq!(inner.instance_id, outer.instance_id);
    assert_eq!(inner.providers.len(), 1);
    assert!(Arc::ptr_eq(&inner.providers[0], &outer.providers[0]));
    for (name, shared) in [
        (
            "hooks",
            Arc::ptr_eq(&inner.dispatch_hook_fn, &outer.dispatch_hook_fn),
        ),
        (
            "work boundary",
            Arc::ptr_eq(
                inner.work_boundary.as_ref().unwrap(),
                outer.work_boundary.as_ref().unwrap(),
            ),
        ),
        (
            "render declarations",
            Arc::ptr_eq(
                &inner.render.as_ref().unwrap().decl_map,
                &outer.render.as_ref().unwrap().decl_map,
            ),
        ),
    ] {
        assert!(shared, "fork must share {name}");
    }
    // Defense in depth: inner calls must never prepare a handoff.
    assert!(
        inner.prepare_handoff_fn.is_none(),
        "fork must clear prepare_handoff_fn"
    );
}

fn assert_copied_metadata(inner: &ToolEvalContext, outer: &ToolEvalContext) {
    assert_eq!(inner.allowed_tool_names, outer.allowed_tool_names);
    assert_eq!(inner.handoff_targets, outer.handoff_targets);
    assert_eq!(inner.current_agent_package, outer.current_agent_package);
}

fn assert_outer_metadata(outer: &ToolEvalContext) {
    assert_eq!(
        (outer.tool_round, outer.current_agent_package.as_deref()),
        (Some(1), Some("pantheon"))
    );
    assert!(outer.allowed_tool_names.contains("atlas_session_handoff"));
    assert_eq!(outer.handoff_targets["atlas"], "pantheon/atlas");
}

#[test]
fn fork_preserves_outer_context_and_shares_admission_dependencies() {
    let outer = context_with_metadata();
    let original_providers = outer.providers.clone();
    let original_confirm = Arc::clone(&outer.confirm_tool_use_fn);
    let original_emitters = (
        Arc::clone(&outer.emit_tool_call_fn),
        Arc::clone(&outer.emit_tool_result_fn),
        Arc::clone(&outer.emit_tool_blocked_fn),
        Arc::clone(&outer.emit_tool_update_fn),
    );

    for parent_round in [Some(12), None] {
        let mut inner = outer.fork_for_inner_calls(silent_options(
            vec![Arc::clone(&original_providers[0])],
            parent_round,
        ));
        assert_eq!(inner.tool_round, parent_round);
        assert_inner_dependencies(&inner, &outer);
        assert_copied_metadata(&inner, &outer);
        let call = test_call("base_tool");
        assert_eq!(
            (inner.confirm_tool_use_fn)(&call, &json!({}), None),
            ToolUseConfirmation::Defer,
        );
        inner.allowed_tool_names.clear();
        inner.handoff_targets.clear();
        inner.current_agent_package = None;
    }

    assert_outer_metadata(&outer);
    assert_eq!(outer.providers.len(), 2);
    for (current, original) in outer.providers.iter().zip(original_providers) {
        assert!(Arc::ptr_eq(current, &original));
    }
    assert!(Arc::ptr_eq(&outer.confirm_tool_use_fn, &original_confirm));
    assert_eq!(
        (outer.confirm_tool_use_fn)(&test_call("base_tool"), &json!({}), None),
        ToolUseConfirmation::Approve,
    );
    for (name, unchanged) in [
        (
            "Started",
            Arc::ptr_eq(&outer.emit_tool_call_fn, &original_emitters.0),
        ),
        (
            "Completed",
            Arc::ptr_eq(&outer.emit_tool_result_fn, &original_emitters.1),
        ),
        (
            "Blocked",
            Arc::ptr_eq(&outer.emit_tool_blocked_fn, &original_emitters.2),
        ),
        (
            "progress",
            Arc::ptr_eq(&outer.emit_tool_update_fn, &original_emitters.3),
        ),
    ] {
        assert!(unchanged, "fork replaced outer {name} callback");
    }
}

#[tokio::test]
async fn fork_defers_approval_without_dispatch_or_changing_outer_consent() {
    let calls = Arc::new(Mutex::new(0));
    let hooks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_hooks = Arc::clone(&hooks);
    let mut outer = test_context(
        vec![Arc::new(MockToolProvider::ok_with_call_count(
            "tool_a",
            Duration::ZERO,
            json!({"ok": true}),
            Arc::clone(&calls),
        ))],
        move |event| {
            captured_hooks.lock().unwrap().push(event.event_name());
            if matches!(event, HookEvent::PreToolUse { .. }) {
                HookOutcome {
                    control: HookResultControl::Ask {
                        reason: Some("approval needed".into()),
                    },
                    result: HookResult::default(),
                }
            } else {
                continue_hook_outcome()
            }
        },
    );
    let confirmations = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&confirmations);
    outer.confirm_tool_use_fn = Arc::new(move |_, _, _| {
        captured.fetch_add(1, Ordering::SeqCst);
        ToolUseConfirmation::Approve
    });
    let inner = outer.fork_for_inner_calls(silent_options(outer.providers.clone(), Some(12)));
    let abort = create_abort_signal();
    let error = eval_inner_tool_call(&inner, test_call("tool_a"), &abort)
        .await
        .unwrap_err();
    let approval = error.downcast_ref::<ToolApprovalRequiredError>().unwrap();
    assert_eq!(approval.deferred_calls().len(), 1);
    assert_eq!(
        approval.deferred_calls()[0].reason.as_deref(),
        Some("approval needed")
    );
    assert_eq!(*calls.lock().await, 0);
    assert_eq!(confirmations.load(Ordering::SeqCst), 0);
    assert_eq!(*hooks.lock().unwrap(), ["PreToolUse"]);

    let result = eval_inner_tool_call(&outer, test_call("tool_a"), &abort)
        .await
        .unwrap()
        .result;
    assert_eq!(result.output, json!({"ok": true}));
    assert_eq!(*calls.lock().await, 1);
    assert_eq!(confirmations.load(Ordering::SeqCst), 1);
    assert_eq!(
        *hooks.lock().unwrap(),
        ["PreToolUse", "PreToolUse", "PostToolUse"]
    );
}

type Trace = Arc<std::sync::Mutex<Vec<(&'static str, String, Value)>>>;

fn collector(trace: &Trace, kind: &'static str) -> Arc<ToolCallEmitFn> {
    let trace = Arc::clone(trace);
    Arc::new(move |call, value| {
        trace.lock().unwrap().push((
            kind,
            call.id.clone().expect("call ID assigned"),
            value.clone(),
        ));
    })
}

#[tokio::test]
async fn fork_routes_lifecycle_and_progress_to_collectors_with_parent_round() {
    let provider = Arc::new(UpdatingToolProvider::default());
    let origins = Arc::clone(&provider.origins);
    let outer = test_context_with_emitters(
        vec![provider, Arc::new(MockToolProvider::panic("blocked_tool"))],
        |event| match event {
            HookEvent::PreToolUse { tool_name, .. } if tool_name == "blocked_tool" => HookOutcome {
                control: HookResultControl::Block {
                    reason: "blocked".into(),
                },
                result: HookResult::default(),
            },
            _ => continue_hook_outcome(),
        },
        |_, _| panic!("inner Started reached outer UI"),
        |_, _| panic!("inner Completed reached outer UI"),
        |_, _| panic!("inner Blocked reached outer UI"),
        |_, _| panic!("inner progress reached outer UI"),
    );
    let trace = Trace::default();
    let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured_updates = Arc::clone(&updates);
    let origin = ToolCallOrigin {
        tool_call_id: Some("parent-call"),
        tool_round: Some(12),
    };
    let inner = outer.fork_for_inner_calls(InnerToolEvalOptions {
        providers: outer.providers.clone(),
        tool_round: origin.tool_round,
        emit_tool_call_fn: collector(&trace, "started"),
        emit_tool_result_fn: collector(&trace, "completed"),
        emit_tool_blocked_fn: collector(&trace, "blocked"),
        emit_tool_update_fn: Arc::new(move |id, patch| {
            captured_updates
                .lock()
                .unwrap()
                .push((id.to_owned(), patch.clone()));
        }),
    });
    let abort = create_abort_signal();
    let success = eval_inner_tool_call(&inner, test_call("updating_tool"), &abort)
        .await
        .unwrap()
        .result;
    let blocked = eval_inner_tool_call(&inner, test_call("blocked_tool"), &abort)
        .await
        .unwrap()
        .result;
    let success_id = success.call.id.unwrap();
    let blocked_id = blocked.call.id.unwrap();

    assert_eq!(
        *trace.lock().unwrap(),
        [
            ("started", success_id.clone(), json!({})),
            ("completed", success_id.clone(), json!({"ok": true})),
            (
                "blocked",
                blocked_id,
                json!({"error": "blocked", "blocked_by_hook": true})
            ),
        ]
    );
    let updates = updates.lock().unwrap();
    assert!(!updates.is_empty());
    assert!(updates.iter().all(|(id, _)| id == &success_id));
    assert_eq!(updates.last().unwrap().1.title.as_deref(), Some("second"));
    assert_eq!(*origins.lock().unwrap(), [(Some(success_id), Some(12))]);
    assert_eq!(outer.tool_round, None);
}

#[tokio::test]
async fn excluded_provider_is_unreachable_in_fork_but_still_available_to_outer() {
    let calls = Arc::new(Mutex::new(0));
    let outer = test_context(
        vec![
            Arc::new(MockToolProvider::ok(
                "base_tool",
                Duration::ZERO,
                json!({"base": true}),
            )),
            Arc::new(MockToolProvider::ok_with_call_count(
                "bash",
                Duration::ZERO,
                json!({"outer": true}),
                Arc::clone(&calls),
            )),
        ],
        |_| continue_hook_outcome(),
    );
    let base_providers = vec![Arc::clone(&outer.providers[0])];
    let inner = outer.fork_for_inner_calls(silent_options(base_providers, Some(12)));
    let abort = create_abort_signal();
    let excluded = eval_inner_tool_call(&inner, test_call("bash"), &abort)
        .await
        .unwrap()
        .result;
    assert_eq!(
        excluded.output,
        json!({"is_error": true, "error": "No tool provider configured for 'bash'"})
    );
    assert_eq!(*calls.lock().await, 0);
    let base = eval_inner_tool_call(&inner, test_call("base_tool"), &abort)
        .await
        .unwrap()
        .result;
    assert_eq!(base.output, json!({"base": true}));
    let parent = eval_inner_tool_call(&outer, test_call("bash"), &abort)
        .await
        .unwrap()
        .result;
    assert_eq!(parent.output, json!({"outer": true}));
    assert_eq!(*calls.lock().await, 1);
}

#[tokio::test]
async fn fork_keeps_work_boundary_checks_and_parent_cancellation() {
    let mut outer = test_context(vec![Arc::new(MockToolProvider::panic("tool_a"))], |_| {
        continue_hook_outcome()
    });
    let boundaries = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&boundaries);
    outer.work_boundary = Some(Arc::new(move |boundary, _| {
        let captured = Arc::clone(&captured);
        Box::pin(async move {
            captured.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!matches!(boundary, WorkBoundary::Start), "lease lost");
            Ok(())
        })
    }));
    let inner = outer.fork_for_inner_calls(silent_options(outer.providers.clone(), Some(12)));
    let abort = create_abort_signal();
    let error = eval_inner_tool_call(&inner, test_call("tool_a"), &abort)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "lease lost");
    let boundary_count = boundaries.load(Ordering::SeqCst);
    assert!(boundary_count > 0);

    abort.set_ctrlc();
    for ctx in [&inner, &outer] {
        let error = eval_inner_tool_call(ctx, test_call("tool_a"), &abort)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "interrupted during tool execution");
    }
    assert_eq!(boundaries.load(Ordering::SeqCst), boundary_count);
}
