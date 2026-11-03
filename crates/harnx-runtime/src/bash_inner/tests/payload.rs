use super::*;
use harnx_engine::tool::WorkBoundary;

const MAX_BYTES: usize = 120;

fn request_size(_tool: &str, args: &Value) -> usize {
    serde_json::to_vec(args).expect("args serialize").len() + 64
}

fn mutating_context(arguments: Value) -> (Arc<FakeToolProvider>, ToolEvalContext, HookEvents) {
    let provider = Arc::new(FakeToolProvider {
        log: Arc::new(DispatchLog::default()),
        handles: None,
        progress_title: None,
        behavior: Behavior::Ok(json!("done")),
    });
    let (ctx, hooks) = outer_context(shared(&provider), HookControl::Mutate(arguments));
    (provider, ctx, hooks)
}

fn limited_executor(provider: &Arc<FakeToolProvider>) -> InnerCallExecutor {
    InnerCallExecutor::new(
        InnerCallExecutorConfig::new(vec!["tool_a".into()], shared(provider)).with_payload_limit(
            PayloadLimit {
                max_bytes: MAX_BYTES,
                request_size: Arc::new(request_size),
            },
        ),
    )
}

fn assert_payload_rejection(outcome: InnerOutcome, bytes: usize) {
    let InnerOutcome::Rejected { reason } = outcome else {
        panic!("expected payload rejection, got {outcome:?}");
    };
    assert_eq!(
        reason,
        format!(
            "tool 'tool_a' request is {bytes} bytes, above the broker's {MAX_BYTES} byte maximum payload"
        )
    );
}

fn assert_rejected_trace(run: &InnerCallRun, bytes: usize) {
    let trace = run.trace();
    let [entry] = trace.as_slice() else {
        panic!("expected one rejected attempt: {trace:?}");
    };
    assert_eq!(
        (entry.name.as_str(), entry.status, entry.bytes),
        ("tool_a", InnerTraceStatus::Rejected, bytes as u64)
    );
}

#[tokio::test]
async fn hook_expanded_payload_is_rejected_before_dispatch() {
    let expanded = json!({"blob": "x".repeat(200)});
    let bytes = request_size("tool_a", &expanded);
    assert!(request_size("tool_a", &json!({})) < MAX_BYTES);
    let (provider, ctx, hooks) = mutating_context(expanded);
    let executor = limited_executor(&provider);
    let progress = Arc::new(RecordingProgress::default());
    let observer = Arc::new(InnerProgressAggregator::new(progress.clone()));
    let run = executor.begin_run(&ctx, PARENT_ROUND, Some(observer));

    let outcome = run.call("tool_a", json!({}), &create_abort_signal()).await;

    assert_payload_rejection(outcome, bytes);
    assert_rejected_trace(&run, bytes);
    assert_eq!(provider.log.count(), 0, "oversized input must not dispatch");
    assert!(matches!(
        hooks.lock().unwrap().as_slice(),
        [HookEvent::PreToolUse { .. }]
    ));
    let patches = progress.patches.lock().unwrap();
    let title = patches.last().unwrap().title.as_deref().unwrap();
    assert_eq!(title_counts(title), (1, 0), "no stranded running call");
}

fn assert_mutated_dispatch(provider: &FakeToolProvider, hooks: &[HookEvent], arguments: &Value) {
    let calls = provider.log.calls.lock().unwrap();
    let [call] = calls.as_slice() else {
        panic!("expected one provider dispatch: {calls:?}");
    };
    assert_eq!(&call.args, arguments);
    let [HookEvent::PreToolUse { tool_input, .. }, HookEvent::PostToolUse {
        tool_input: post_input,
        ..
    }] = hooks
    else {
        panic!("expected one pre and one post hook: {hooks:?}");
    };
    assert_eq!(tool_input, &json!({}));
    assert_eq!(post_input, arguments);
}

#[tokio::test]
async fn hook_mutated_payload_at_or_below_limit_dispatches_final_arguments() {
    let overhead = request_size("tool_a", &json!({"blob": ""}));
    for bytes in [MAX_BYTES - 1, MAX_BYTES] {
        let arguments = json!({"blob": "x".repeat(bytes - overhead)});
        assert_eq!(request_size("tool_a", &arguments), bytes);
        let (provider, ctx, hooks) = mutating_context(arguments.clone());
        let executor = limited_executor(&provider);
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);

        let outcome = run.call("tool_a", json!({}), &create_abort_signal()).await;

        assert!(matches!(outcome, InnerOutcome::Ok { .. }), "{outcome:?}");
        assert_mutated_dispatch(&provider, &hooks.lock().unwrap(), &arguments);
    }
}

#[tokio::test]
async fn payload_check_preserves_existing_work_boundary_and_fatal_errors() {
    for fail_start in [false, true] {
        let arguments = json!({"injected": true});
        let (provider, mut ctx, _) = mutating_context(arguments.clone());
        let boundaries = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&boundaries);
        ctx.work_boundary = Some(Arc::new(move |boundary, call| {
            let start = matches!(boundary, WorkBoundary::Start);
            captured.lock().unwrap().push((start, call.arguments));
            Box::pin(async move {
                anyhow::ensure!(!(start && fail_start), "dispatch fence closed");
                Ok(())
            })
        }));
        let original = ctx.work_boundary.clone().unwrap();
        let executor = limited_executor(&provider);
        let run = executor.begin_run(&ctx, PARENT_ROUND, None);

        let outcome = run.call("tool_a", json!({}), &create_abort_signal()).await;

        assert_boundary_outcome(outcome, fail_start);
        assert_eq!(provider.log.count(), usize::from(!fail_start));
        assert_boundary_arguments(&boundaries.lock().unwrap(), &arguments);
        assert!(Arc::ptr_eq(ctx.work_boundary.as_ref().unwrap(), &original));
    }
}

fn assert_boundary_outcome(outcome: InnerOutcome, fail_start: bool) {
    match (fail_start, outcome) {
        (false, InnerOutcome::Ok { .. }) => {}
        (true, InnerOutcome::Fatal { message }) => assert_eq!(message, "dispatch fence closed"),
        (_, other) => panic!("unexpected boundary outcome: {other:?}"),
    }
}

fn assert_boundary_arguments(boundaries: &[(bool, Value)], arguments: &Value) {
    assert_eq!(boundaries.first(), Some(&(false, json!({}))));
    let starts: Vec<_> = boundaries
        .iter()
        .filter_map(|(start, args)| start.then_some(args))
        .collect();
    assert_eq!(starts, [arguments]);
}

#[tokio::test]
async fn hook_expanded_payload_without_limit_is_unchanged() {
    let arguments = json!({"blob": "x".repeat(200)});
    let (provider, ctx, hooks) = mutating_context(arguments.clone());
    let executor = executor_with(&["tool_a"], shared(&provider), (8, 100));
    let run = executor.begin_run(&ctx, PARENT_ROUND, None);

    let outcome = run.call("tool_a", json!({}), &create_abort_signal()).await;

    assert!(matches!(outcome, InnerOutcome::Ok { .. }), "{outcome:?}");
    assert_mutated_dispatch(&provider, &hooks.lock().unwrap(), &arguments);
}
