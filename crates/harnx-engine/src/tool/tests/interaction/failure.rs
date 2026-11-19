use super::*;

#[tokio::test]
async fn interaction_manifest_failure_retains_completed_and_pending_calls() {
    let mut ctx = test_context(vec![], |_| continue_hook_outcome());
    let events: Events = Default::default();
    callbacks(&mut ctx, &events);
    ctx.middleware_fn = Some(Arc::new(|prepared, _, _| {
        Box::pin(async move {
            if prepared.call.name == "a" {
                return Ok(input_required());
            }
            Ok(ToolMiddlewareOutcome::Completed(
                MaterializedToolResult::new(
                    ToolResult::new(prepared.call, json!("done")),
                    InnerToolCallStatus::Ok,
                ),
            ))
        })
    }));
    ctx.on_interaction = Some(Arc::new(|_| {
        Box::pin(async { anyhow::bail!("manifest unavailable") })
    }));
    let error = eval_tool_round(
        &ctx,
        vec![call("a"), call("b")],
        vec![],
        &create_abort_signal(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "manifest unavailable");
    let failure = error.downcast_ref::<ToolRoundFailure>().unwrap();
    assert_eq!(failure.completed[0].call.id.as_deref(), Some("b"));
    assert_eq!(failure.pending[0].call.id.as_deref(), Some("a"));
    assert_eq!(*events.lock().unwrap(), ["complete:b", "publish:b"]);
}

#[tokio::test]
async fn interaction_fatal_sibling_retains_unexposed_pending_request() {
    let mut ctx = test_context(vec![], |_| continue_hook_outcome());
    ctx.middleware_fn = Some(Arc::new(|prepared, _, _| {
        Box::pin(async move {
            if prepared.call.name == "a" {
                Err(ToolError::Fatal(Fatal::other("fatal a").into()))
            } else {
                Ok(input_required())
            }
        })
    }));
    ctx.on_interaction = Some(Arc::new(|_| {
        panic!("a fatal sibling must be classified before showing input")
    }));
    let error = eval_tool_round(
        &ctx,
        vec![call("a"), call("b")],
        vec![],
        &create_abort_signal(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "fatal a");
    assert!(error.downcast_ref::<Fatal>().is_some());
    let failure = error.downcast_ref::<ToolRoundFailure>().unwrap();
    assert!(failure.completed.is_empty());
    assert_eq!(failure.pending[0].call.id.as_deref(), Some("b"));
}

#[tokio::test]
async fn interaction_legacy_opt_in_preserves_typed_suspension() {
    let mut ctx = test_context(vec![], |_| continue_hook_outcome());
    ctx.middleware_fn = Some(Arc::new(|_, _, _| Box::pin(async { Ok(input_required()) })));
    ctx.on_interaction = Some(Arc::new(|_| Box::pin(async { Ok(()) })));
    let error = eval_tool_calls(&ctx, vec![call("a")], &create_abort_signal())
        .await
        .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<ToolInputRequiredError>()
            .unwrap()
            .0
            .pending[0]
            .call
            .id
            .as_deref(),
        Some("a")
    );
}
