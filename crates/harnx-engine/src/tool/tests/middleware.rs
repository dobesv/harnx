use super::*;
use std::sync::Mutex as StdMutex;

fn completed(outcome: ToolMiddlewareOutcome) -> MaterializedToolResult {
    match outcome {
        ToolMiddlewareOutcome::Completed(result) => result,
        ToolMiddlewareOutcome::InputRequired { .. } => panic!("unexpected interaction"),
        ToolMiddlewareOutcome::ToolInputRequired(_) => panic!("unexpected tool challenge"),
    }
}

#[tokio::test]
async fn middleware_rewrites_skips_and_repeats_with_one_outer_publication() {
    for count in 0..=2 {
        let arguments = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(StdMutex::new(Vec::<String>::new()));
        let mut ctx = test_context(vec![Arc::new(CapturingToolProvider::new(
            "tool",
            arguments.clone(),
            json!({"raw": true}),
        ))]);
        let published = events.clone();
        ctx.emit_tool_result_fn = Arc::new(move |_, result| {
            assert_eq!(result, &json!({"effective": count}));
            published.lock().unwrap().push("publish".into());
        });
        let starts = Arc::new(StdMutex::new(Vec::new()));
        let captured = starts.clone();
        ctx.work_boundary = Some(Arc::new(move |boundary, call| {
            if matches!(boundary, WorkBoundary::Start) {
                captured.lock().unwrap().push(call.arguments);
            }
            Box::pin(async { Ok(()) })
        }));
        let chain_events = events.clone();
        ctx.middleware_fn = Some(Arc::new(move |prepared, origin, next| {
            let events = chain_events.clone();
            Box::pin(async move {
                assert_eq!(origin, ToolMiddlewareOrigin::Model);
                assert_eq!(prepared.arguments, json!({}));
                events.lock().unwrap().push("enter".into());
                let mut result = MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!({"skipped": true})),
                    InnerToolCallStatus::Ok,
                );
                for attempt in 0..count {
                    result = completed(next(json!({"attempt": attempt})).await?);
                    assert_eq!(result.result.output, json!({"raw": true}));
                    assert!(!events.lock().unwrap().iter().any(|e| e == "publish"));
                }
                result.result.output = json!({"effective": count});
                events.lock().unwrap().push("exit".into());
                Ok(ToolMiddlewareOutcome::Completed(result))
            })
        }));
        let mut original = test_call("tool");
        ensure_tool_call_ids(std::slice::from_mut(&mut original));
        let results = eval_tool_calls(&ctx, vec![original.clone()], &create_abort_signal())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call, original);
        assert_eq!(results[0].output, json!({"effective": count}));
        let expected_arguments = (0..count)
            .map(|attempt| json!({"attempt": attempt}))
            .collect::<Vec<_>>();
        assert_eq!(*arguments.lock().await, expected_arguments);
        assert_eq!(*starts.lock().unwrap(), expected_arguments);
        let mut expected = vec!["enter"];
        expected.extend(["exit", "publish"]);
        assert_eq!(*events.lock().unwrap(), expected);
    }
}

#[tokio::test]
async fn middleware_receives_inner_and_operator_origins() {
    for origin in [ToolMiddlewareOrigin::Inner, ToolMiddlewareOrigin::Operator] {
        let mut ctx = test_context(vec![]);
        ctx.allowed_tool_names.insert("tool".into());
        ctx.middleware_fn = Some(Arc::new(move |prepared, actual, _| {
            Box::pin(async move {
                assert_eq!(actual, origin);
                Ok(ToolMiddlewareOutcome::Completed(
                    MaterializedToolResult::new(
                        ToolResult::new(prepared.call, json!({"origin": format!("{actual:?}")})),
                        InnerToolCallStatus::Ok,
                    ),
                ))
            })
        }));
        let result = match origin {
            ToolMiddlewareOrigin::Inner => {
                eval_inner_tool_call(&ctx, test_call("tool"), &create_abort_signal())
                    .await
                    .unwrap()
                    .result
            }
            ToolMiddlewareOrigin::Operator => {
                eval_operator_tool_call(ctx, test_call("tool"), &create_abort_signal())
                    .await
                    .unwrap()
            }
            _ => unreachable!(),
        };
        assert_eq!(result.output["origin"], format!("{origin:?}"));
    }
}

#[tokio::test]
async fn middleware_cannot_swallow_typed_fatal_dispatch() {
    let mut ctx = test_context(vec![Arc::new(MockToolProvider::err(
        "tool",
        Duration::ZERO,
        ToolError::Fatal(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()),
    ))]);
    ctx.middleware_fn = Some(Arc::new(|prepared, _, next| {
        Box::pin(async move {
            let _ = next(prepared.arguments).await;
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!("swallowed")),
                    InnerToolCallStatus::Ok,
                ),
            ))
        })
    }));
    let error = eval_tool_calls(&ctx, vec![test_call("tool")], &create_abort_signal())
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[tokio::test]
async fn middleware_cancellation_between_next_calls_prevents_second_dispatch() {
    let arguments = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = test_context(vec![Arc::new(CapturingToolProvider::new(
        "tool",
        arguments.clone(),
        json!("ok"),
    ))]);
    let abort = create_abort_signal();
    let cancel = abort.clone();
    ctx.middleware_fn = Some(Arc::new(move |prepared, _, next| {
        let cancel = cancel.clone();
        Box::pin(async move {
            next(json!({"first": true})).await?;
            cancel.set_ctrlc();
            next(prepared.arguments).await
        })
    }));
    assert!(eval_tool_calls(&ctx, vec![test_call("tool")], &abort)
        .await
        .unwrap_err()
        .to_string()
        .contains("interrupted"));
    assert_eq!(*arguments.lock().await, [json!({"first": true})]);
}

#[tokio::test]
async fn middleware_revalidates_ownership_before_every_next() {
    struct Ownership(AtomicUsize);
    impl harnx_core::tool::ReplayAuthorization for Ownership {
        fn revalidate<'a, 'async_trait>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'async_trait>>
        where
            'a: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
                }
            })
        }
    }
    let arguments = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = test_context(vec![Arc::new(CapturingToolProvider::new(
        "tool",
        arguments.clone(),
        json!("ok"),
    ))]);
    ctx.middleware_fn = Some(Arc::new(|prepared, _, next| {
        Box::pin(async move {
            next(prepared.arguments.clone()).await?;
            next(prepared.arguments).await
        })
    }));
    let ownership = Ownership(AtomicUsize::new(0));
    let error = eval_tool_calls_with_authorization(
        &ctx,
        vec![test_call("tool")],
        &create_abort_signal(),
        Some(&ownership),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(ownership.0.load(Ordering::SeqCst), 2);
    assert_eq!(arguments.lock().await.len(), 1);
}

#[tokio::test]
async fn middleware_handoff_snapshot_survives_only_non_error_outer_result() {
    for error_key in [None, Some("is_error"), Some("isError")] {
        let mut ctx = test_context(vec![]);
        ctx.allowed_tool_names
            .insert("atlas_session_handoff".into());
        ctx.middleware_fn = Some(Arc::new(move |prepared, _, next| {
            Box::pin(async move {
                let mut result = completed(next(prepared.arguments).await?);
                assert_eq!(result.result.switch_agent.as_ref().unwrap().agent, "atlas");
                if let Some(key) = error_key {
                    result.result.output = json!({key: true, "error": "discarded after dispatch"});
                }
                Ok(ToolMiddlewareOutcome::Completed(result))
            })
        }));
        let call = ToolCall::new(
            "atlas_session_handoff".into(),
            json!({"prompt": "task"}),
            Some("handoff".into()),
            None,
        );
        let results = eval_tool_calls(&ctx, vec![call], &create_abort_signal())
            .await
            .unwrap();
        assert_eq!(results[0].switch_agent.is_some(), error_key.is_none());
    }
}

#[tokio::test]
async fn middleware_next_retains_private_execution_context() {
    let observation = harnx_core::execution_context::ExecutionContextObservation::observe(
        std::path::Path::new("/private"),
        std::path::Path::new("/private"),
    );
    let mut ctx = test_context(vec![Arc::new(PrivateContextProvider {
        observation: observation.clone(),
    })]);
    ctx.middleware_fn = Some(Arc::new(move |prepared, _, next| {
        let observation = observation.clone();
        Box::pin(async move {
            let result = completed(next(prepared.arguments).await?);
            assert_eq!(result.result.execution_context, Some(observation));
            Ok(ToolMiddlewareOutcome::Completed(result))
        })
    }));
    let results = eval_tool_calls(
        &ctx,
        vec![test_call("context_tool")],
        &create_abort_signal(),
    )
    .await
    .unwrap();
    assert!(results[0].execution_context.is_some());
    assert!(!serde_json::to_string(&results[0])
        .unwrap()
        .contains("/private"));
}

#[tokio::test(start_paused = true)]
async fn middleware_is_polled_while_next_is_pending() {
    let mut ctx = test_context(vec![Arc::new(MockToolProvider::ok(
        "tool",
        Duration::from_secs(100),
        json!("too late"),
    ))]);
    ctx.middleware_fn = Some(Arc::new(|prepared, _, next| {
        Box::pin(async move {
            tokio::select! {
                result = next(prepared.arguments) => result,
                _ = tokio::time::sleep(Duration::from_secs(1)) => Err(ToolError::Recoverable(anyhow!("middleware timed out"))),
            }
        })
    }));
    let started = Instant::now();
    let results = eval_tool_calls(&ctx, vec![test_call("tool")], &create_abort_signal())
        .await
        .unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    assert_eq!(results[0].output["error"], "middleware timed out");
}

#[tokio::test]
async fn escaped_middleware_continuation_cannot_dispatch() {
    let saved: Arc<StdMutex<Option<Arc<ToolCallNext>>>> = Arc::default();
    let captured = saved.clone();
    let mut ctx = test_context(vec![Arc::new(MockToolProvider::panic("tool"))]);
    ctx.middleware_fn = Some(Arc::new(move |prepared, _, next| {
        *captured.lock().unwrap() = Some(next);
        Box::pin(async move {
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!("skipped")),
                    InnerToolCallStatus::Ok,
                ),
            ))
        })
    }));
    eval_tool_calls(&ctx, vec![test_call("tool")], &create_abort_signal())
        .await
        .unwrap();
    let next = saved.lock().unwrap().take().unwrap();
    assert!(matches!(next(json!({})).await, Err(ToolError::Fatal(_))));
}

#[tokio::test]
async fn middleware_next_materializes_images_before_callback_unwinds() {
    let mut ctx = test_context(vec![Arc::new(CapturingToolProvider::new(
        "tool",
        Arc::default(),
        json!({"content": [{"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}]}),
    ))]);
    ctx.middleware_fn = Some(Arc::new(|prepared, _, next| {
        Box::pin(async move {
            let result = completed(next(prepared.arguments).await?);
            assert_eq!(result.result.content.len(), 1);
            assert!(!result.result.output.to_string().contains("aGVsbG8="));
            Ok(ToolMiddlewareOutcome::Completed(result))
        })
    }));
    let results = eval_tool_calls(&ctx, vec![test_call("tool")], &create_abort_signal())
        .await
        .unwrap();
    assert_eq!(results[0].content.len(), 1);
}

#[tokio::test]
async fn middleware_next_materializes_null_without_publishing_it() {
    for rewrite in [false, true] {
        let mut ctx = test_context(vec![Arc::new(MockToolProvider::ok(
            "tool",
            Duration::ZERO,
            Value::Null,
        ))]);
        let published = Arc::new(StdMutex::new(Vec::new()));
        let captured = published.clone();
        ctx.emit_tool_result_fn =
            Arc::new(move |_, value| captured.lock().unwrap().push(value.clone()));
        ctx.middleware_fn = Some(Arc::new(move |prepared, _, next| {
            Box::pin(async move {
                let mut result = completed(next(prepared.arguments).await?);
                assert_eq!(result.result.output, json!("DONE"));
                if rewrite {
                    result.result.output = json!("rewritten");
                }
                Ok(ToolMiddlewareOutcome::Completed(result))
            })
        }));
        let results = eval_tool_calls(&ctx, vec![test_call("tool")], &create_abort_signal())
            .await
            .unwrap();
        if rewrite {
            assert_eq!(results[0].output, json!("rewritten"));
            assert_eq!(*published.lock().unwrap(), [json!("rewritten")]);
        } else {
            assert!(results.is_empty());
            assert_eq!(*published.lock().unwrap(), [Value::Null]);
        }
    }
}

#[tokio::test]
async fn middleware_handoff_round_waits_for_sibling_chain() {
    let ready = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let callback_ready = ready.clone();
    let callback_release = release.clone();
    let mut ctx = test_context(vec![]);
    ctx.allowed_tool_names
        .insert("atlas_session_handoff".into());
    ctx.middleware_fn = Some(Arc::new(move |prepared, _, next| {
        let ready = callback_ready.clone();
        let release = callback_release.clone();
        Box::pin(async move {
            if prepared.call.name == "sibling" {
                release.notified().await;
                Ok(ToolMiddlewareOutcome::Completed(
                    MaterializedToolResult::new(
                        ToolResult::new(prepared.call, json!("sibling complete")),
                        InnerToolCallStatus::Ok,
                    ),
                ))
            } else {
                let result = next(prepared.arguments).await?;
                ready.notify_one();
                Ok(result)
            }
        })
    }));
    let round = tokio::spawn(async move {
        let handoff = ToolCall::new(
            "atlas_session_handoff".into(),
            json!({"prompt": "task"}),
            Some("handoff".into()),
            None,
        );
        eval_tool_calls(
            &ctx,
            vec![handoff, test_call("sibling")],
            &create_abort_signal(),
        )
        .await
    });
    ready.notified().await;
    assert!(
        !round.is_finished(),
        "round must not select/activate a handoff while its sibling waits"
    );
    release.notify_one();
    let results = round.await.unwrap().unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].switch_agent.is_some());
    assert_eq!(results[1].output, json!("sibling complete"));
}

#[tokio::test]
async fn middleware_rechecks_work_admission_with_effective_arguments() {
    let arguments = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = test_context(vec![Arc::new(CapturingToolProvider::new(
        "tool",
        arguments.clone(),
        json!("ok"),
    ))]);
    ctx.work_boundary = Some(Arc::new(|boundary, call| {
        Box::pin(async move {
            if matches!(boundary, WorkBoundary::Start) && call.arguments["denied"] == true {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into());
            }
            Ok(())
        })
    }));
    ctx.middleware_fn = Some(Arc::new(|_, _, next| {
        Box::pin(async move {
            next(json!({"denied": false})).await?;
            next(json!({"denied": true})).await
        })
    }));
    let error = eval_tool_calls(&ctx, vec![test_call("tool")], &create_abort_signal())
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(*arguments.lock().await, [json!({"denied": false})]);
}

#[tokio::test]
async fn middleware_survives_inner_context_fork() {
    let mut outer = test_context(vec![]);
    outer.middleware_fn = Some(Arc::new(|prepared, origin, _| {
        Box::pin(async move {
            assert_eq!(origin, ToolMiddlewareOrigin::Inner);
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!("wrapped inner")),
                    InnerToolCallStatus::Ok,
                ),
            ))
        })
    }));
    let inner = outer.fork_for_inner_calls(InnerToolEvalOptions {
        providers: vec![],
        tool_round: None,
        emit_tool_call_fn: Arc::new(|_, _| {}),
        emit_tool_result_fn: Arc::new(|_, _| {}),
        emit_tool_blocked_fn: Arc::new(|_, _| {}),
        emit_tool_update_fn: Arc::new(|_, _| {}),
    });
    assert!(Arc::ptr_eq(
        inner.middleware_fn.as_ref().unwrap(),
        outer.middleware_fn.as_ref().unwrap()
    ));
    let result = eval_inner_tool_call(&inner, test_call("tool"), &create_abort_signal())
        .await
        .unwrap();
    assert_eq!(result.result.output, json!("wrapped inner"));
}

#[tokio::test]
async fn middleware_next_preserves_host_operation_task_context() {
    tokio::task_local! { static SCRIPT_DEPTH: usize; }
    let mut ctx = test_context(vec![Arc::new(MockToolProvider::ok(
        "tool",
        Duration::ZERO,
        json!("ok"),
    ))]);
    ctx.work_boundary = Some(Arc::new(|boundary, _| {
        Box::pin(async move {
            if matches!(boundary, WorkBoundary::Start) {
                assert_eq!(
                    SCRIPT_DEPTH.get(),
                    7,
                    "next must run in its host operation's task-local scope"
                );
            }
            Ok(())
        })
    }));
    ctx.middleware_fn = Some(Arc::new(|prepared, _, next| {
        Box::pin(async move { SCRIPT_DEPTH.scope(7, next(prepared.arguments)).await })
    }));
    let results = eval_tool_calls(&ctx, vec![test_call("tool")], &create_abort_signal())
        .await
        .unwrap();
    assert_eq!(results[0].output, json!("ok"));
}
