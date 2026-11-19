mod failure;

use super::*;
use harnx_core::interaction::{confirm_surface, InteractionRequest};
use std::sync::Mutex as StdMutex;
use tokio::sync::Notify;

type Events = Arc<StdMutex<Vec<String>>>;

fn call(name: &str) -> ToolCall {
    ToolCall::new(name.into(), json!({}), Some(name.into()), None)
}

fn input_required() -> ToolMiddlewareOutcome {
    ToolMiddlewareOutcome::InputRequired {
        interaction: InteractionRequest {
            interaction_id: "interaction-a".into(),
            producer: "middleware:confirm".into(),
            step_path: vec![],
            key: "keep".into(),
            kind: "confirm".into(),
            payload: confirm_surface("surface", "Keep?"),
            summary: "Keep?".into(),
            chain_version: "v1".into(),
        },
        checkpoint: ToolCallCheckpoint {
            state: json!({"stash": {"kept": 42}}),
            ..Default::default()
        },
    }
}

fn callbacks(ctx: &mut ToolEvalContext, events: &Events) {
    let completed = events.clone();
    ctx.on_tool_call_completed = Some(Arc::new(move |mut result| {
        let events = completed.clone();
        Box::pin(async move {
            events
                .lock()
                .unwrap()
                .push(format!("complete:{}", result.call.name));
            result.markdown = Some("rendered before checkpoint".into());
            Ok(result)
        })
    }));
    let published = events.clone();
    ctx.emit_tool_result_fn = Arc::new(move |call, _| {
        published
            .lock()
            .unwrap()
            .push(format!("publish:{}", call.name))
    });
    let interactions = events.clone();
    ctx.on_interaction = Some(Arc::new(move |round| {
        let events = interactions.clone();
        Box::pin(async move {
            assert_eq!(round.completed.len(), 1);
            assert_eq!(round.completed[0].call.name, "b");
            assert_eq!(
                round.completed[0].markdown.as_deref(),
                Some("rendered before checkpoint")
            );
            assert_eq!(round.pending[0].interaction.interaction_id, "interaction-a");
            events.lock().unwrap().push("interact:a".into());
            Ok(())
        })
    }));
}

#[tokio::test]
async fn interaction_suspension_keeps_completed_sibling_and_orders_callbacks() {
    let events: Events = Default::default();
    let release = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let mut ctx = test_context(vec![]);
    callbacks(&mut ctx, &events);
    let (wait, started) = (release.clone(), entered.clone());
    ctx.middleware_fn = Some(Arc::new(move |prepared, _, _| {
        let (wait, started) = (wait.clone(), started.clone());
        Box::pin(async move {
            if prepared.call.name == "a" {
                return Ok(input_required());
            }
            started.notify_one();
            wait.notified().await;
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!("done")),
                    InnerToolCallStatus::Ok,
                ),
            ))
        })
    }));
    let task = tokio::spawn(async move {
        eval_tool_round(
            &ctx,
            vec![call("a"), call("b")],
            vec![],
            &create_abort_signal(),
        )
        .await
    });
    entered.notified().await;
    assert!(
        events.lock().unwrap().is_empty(),
        "interaction visible before sibling settled"
    );
    release.notify_one();
    let ToolRoundOutcome::InputRequired(round) = task.await.unwrap().unwrap() else {
        panic!("expected suspension")
    };
    assert_eq!(
        *events.lock().unwrap(),
        ["complete:b", "publish:b", "interact:a"]
    );
    assert_eq!(round.completed.len(), 1);
    assert_eq!(round.completed[0].output, "done");
    assert_eq!(round.pending.len(), 1);
    assert_eq!(round.pending[0].call.id.as_deref(), Some("a"));
    assert_eq!(round.pending[0].checkpoint.state["stash"]["kept"], 42);
}

#[tokio::test]
async fn interaction_completion_fires_before_slow_sibling_finishes() {
    let finished = Arc::new(Notify::new());
    let wait = finished.clone();
    let mut ctx = test_context(vec![]);
    ctx.middleware_fn = Some(Arc::new(move |prepared, _, _| {
        let wait = wait.clone();
        Box::pin(async move {
            if prepared.call.name == "a" {
                wait.notified().await;
                return Ok(input_required());
            }
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!(true)),
                    InnerToolCallStatus::Ok,
                ),
            ))
        })
    }));
    ctx.on_tool_call_completed = Some(Arc::new(move |result| {
        finished.notify_one();
        Box::pin(async { Ok(result) })
    }));
    ctx.on_interaction = Some(Arc::new(|_| Box::pin(async { Ok(()) })));
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        eval_tool_round(
            &ctx,
            vec![call("a"), call("b")],
            vec![],
            &create_abort_signal(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(outcome, ToolRoundOutcome::InputRequired(_)));
}

#[tokio::test]
async fn interaction_supplied_completed_call_skips_every_execution_stage() {
    let mut ctx = test_context(vec![Arc::new(MockToolProvider::panic("b"))]);
    ctx.on_tool_call_completed = Some(Arc::new(|result| {
        assert_ne!(result.call.name, "b");
        Box::pin(async { Ok(result) })
    }));
    ctx.middleware_fn = Some(Arc::new(|prepared, _, _| {
        Box::pin(async move {
            assert_eq!(prepared.call.name, "a");
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!("new")),
                    InnerToolCallStatus::Ok,
                ),
            ))
        })
    }));
    ctx.emit_tool_result_fn = Arc::new(|call, _| assert_eq!(call.name, "a"));
    let completed = ToolResult::new(call("b"), json!("saved"));
    let ToolRoundOutcome::Completed(results) = eval_tool_round(
        &ctx,
        vec![call("b"), call("a")],
        vec![completed.clone()],
        &create_abort_signal(),
    )
    .await
    .unwrap() else {
        panic!("expected results")
    };
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].call, completed.call);
    assert_eq!(results[0].output, "saved");
    assert_eq!(results[1].output, "new");
}

type Fatal = std::io::Error;

#[tokio::test]
async fn interaction_fatal_sibling_retains_completion_and_original_error() {
    // Both plain provider and middleware dispatch must preserve B's result.
    for middleware in [false, true] {
        let mut ctx = test_context(vec![
            Arc::new(MockToolProvider::err(
                "a",
                Duration::ZERO,
                ToolError::Fatal(Fatal::other("fatal a").into()),
            )),
            Arc::new(MockToolProvider::ok("b", Duration::ZERO, json!("done"))),
        ]);
        if middleware {
            ctx.middleware_fn = Some(Arc::new(|prepared, _, next| {
                Box::pin(async move { next(prepared.arguments).await })
            }));
        }
        let events: Events = Default::default();
        callbacks(&mut ctx, &events);
        let error = eval_tool_round(
            &ctx,
            vec![call("a"), call("b")],
            vec![],
            &create_abort_signal(),
        )
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<Fatal>().is_some());
        let round = error.downcast_ref::<ToolRoundFailure>().unwrap();
        assert_eq!(round.completed.len(), 1);
        assert_eq!(round.completed[0].output, "done");
        assert_eq!(*events.lock().unwrap(), ["complete:b", "publish:b"]);
    }
}

#[tokio::test]
async fn interaction_without_callback_is_recoverable_error_result() {
    let mut ctx = test_context(vec![]);
    ctx.middleware_fn = Some(Arc::new(|_, _, _| Box::pin(async { Ok(input_required()) })));
    let results = eval_tool_calls(&ctx, vec![call("a")], &create_abort_signal())
        .await
        .unwrap();
    assert_eq!(
        results[0].output,
        json!({"is_error":true, "error":"interaction not supported here"})
    );
    assert!(results[0].switch_agent.is_none());
}

#[tokio::test]
async fn interaction_completion_failure_prevents_publication_but_settles_siblings() {
    let mut ctx = test_context(vec![]);
    ctx.middleware_fn = Some(Arc::new(|prepared, _, _| {
        Box::pin(async move {
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!(true)),
                    InnerToolCallStatus::Ok,
                ),
            ))
        })
    }));
    ctx.on_tool_call_completed = Some(Arc::new(|result| {
        Box::pin(async move {
            if result.call.name == "a" {
                anyhow::bail!("checkpoint unavailable");
            }
            Ok(result)
        })
    }));
    ctx.emit_tool_result_fn = Arc::new(|call, _| assert_eq!(call.name, "b"));
    let error = eval_tool_round(
        &ctx,
        vec![call("a"), call("b")],
        vec![],
        &create_abort_signal(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("checkpoint unavailable"));
    assert_eq!(
        error.downcast_ref::<ToolRoundFailure>().unwrap().completed[0]
            .call
            .name,
        "b"
    );
}

#[tokio::test]
async fn interaction_rejects_mismatched_completed_results_before_dispatch() {
    let ctx = test_context(vec![Arc::new(MockToolProvider::panic("a"))]);
    let error = eval_tool_round(
        &ctx,
        vec![call("a")],
        vec![ToolResult::new(call("b"), json!(true))],
        &create_abort_signal(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("does not match this round"));
}
