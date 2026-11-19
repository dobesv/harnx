use super::*;
use futures_util::future::join_all;
use harnx_core::partial_result::error_with_partial_result;
use std::io::{Error as IoError, ErrorKind};

#[tokio::test]
async fn preserves_outputs_and_lossless_media() {
    for output in [
        Value::Null,
        json!({"ok": true}),
        json!({"isError": true, "content": [{"type": "text", "text": "tool failed"}]}),
        json!({"content": [{"type": "image", "mimeType": "image/png", "data": "Zm9v"}]}),
    ] {
        let ctx = test_context(vec![Arc::new(MockToolProvider::ok(
            "tool_a",
            Duration::ZERO,
            output.clone(),
        ))]);
        let evaluated = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
            .await
            .unwrap();
        let result = evaluated.result;

        assert_eq!(evaluated.status, InnerToolCallStatus::Ok);
        assert_eq!(result.output, output);
        assert!(
            result.content.is_empty(),
            "lossless calls leave media inline"
        );
    }
}

#[tokio::test]
async fn provider_error_keeps_partial_result() {
    let error =
        error_with_partial_result(anyhow!("child failed"), json!({"session_id": "child-1"}));
    let ctx = test_context(vec![Arc::new(MockToolProvider::err(
        "tool_a",
        Duration::ZERO,
        ToolError::Recoverable(error),
    ))]);
    let evaluated = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
        .await
        .unwrap();
    let result = evaluated.result;
    assert_eq!(evaluated.status, InnerToolCallStatus::ToolError);

    assert_eq!(
        result.output,
        json!({
            "is_error": true,
            "error": "child failed",
            "partial_result": {"session_id": "child-1"},
        })
    );
}

#[tokio::test]
async fn fatal_provider_errors_propagate() {
    for kind in [
        ErrorKind::Interrupted,
        ErrorKind::TimedOut,
        ErrorKind::PermissionDenied,
    ] {
        let error = anyhow::Error::new(IoError::from(kind)).context("provider stopped");
        let ctx = test_context(vec![Arc::new(MockToolProvider::err(
            "tool_a",
            Duration::ZERO,
            ToolError::Fatal(error),
        ))]);
        let error = eval_inner_tool_call(&ctx, test_call("tool_a"), &create_abort_signal())
            .await
            .unwrap_err();

        assert_eq!(error.downcast_ref::<IoError>().unwrap().kind(), kind);
        assert_eq!(error.to_string(), "provider stopped");
    }
}

#[tokio::test]
async fn abort_before_or_during_admission_propagates_as_error() {
    let abort = create_abort_signal();
    abort.set_ctrlc();
    let ctx = test_context(vec![Arc::new(MockToolProvider::panic("tool_a"))]);
    let error = eval_inner_tool_call(&ctx, test_call("tool_a"), &abort)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "interrupted during tool execution");
}

#[tokio::test]
async fn work_boundary_errors_propagate_without_provider_dispatch() {
    for kind in [ErrorKind::TimedOut, ErrorKind::PermissionDenied] {
        let mut ctx = test_context(vec![Arc::new(MockToolProvider::panic("tool_a"))]);
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

#[tokio::test]
async fn concurrent_calls_share_context_and_route_progress_by_id() {
    let provider = Arc::new(UpdatingToolProvider::default());
    let origins = Arc::clone(&provider.origins);
    let mut ctx = test_context(vec![provider]);
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
                .result
        })
    });
    let results: Vec<_> = join_all(handles)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_ne!(results[0].call.id, results[1].call.id);
    assert_eq!(ctx.tool_round, Some(12));
    assert_eq!(origins.lock().unwrap().len(), 2);
    for result in results {
        let id = result.call.id.unwrap();
        assert!(origins
            .lock()
            .unwrap()
            .contains(&(Some(id.clone()), Some(12))));
        let updates = updates.lock().unwrap();
        let last = updates.iter().rfind(|(call_id, _)| call_id == &id).unwrap();
        assert_eq!(last.1.title.as_deref(), Some("second"));
    }
}
