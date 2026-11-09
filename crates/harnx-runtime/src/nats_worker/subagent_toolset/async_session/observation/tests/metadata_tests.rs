use super::{fixture::*, *};

#[test]
fn minimum_budget_keeps_escaped_handles_status_and_maximal_error_metadata() {
    // Walk the preflight boundary using identifiers with JSON escape overhead.
    let mut accepted = 0;
    for length in 1..200 {
        let handle = PromptHandle {
            session_id: "é\n\"".repeat(length),
            prompt_id: "\t\\".repeat(length),
        };
        let mut frame = Frame::new(&handle, 1024);
        if frame.preflight(None).is_err() {
            continue;
        }
        accepted += 1;
        frame.result.dispatch = Some(DispatchConfirmation::Unconfirmed);
        frame.result.status = SessionPromptStatus::Pending;
        frame.stopped(Stop::Unavailable);
        let result = frame.finish(&Budget::new(0, CancellationToken::new()));
        assert_eq!(result["session_id"], handle.session_id);
        assert_eq!(result["prompt_id"], handle.prompt_id);
        assert_eq!(result["status"], "unknown");
        assert!(result["output"]["observation"].is_object());
        assert!(serialized(&result) <= 1024);
    }
    assert!(accepted > 1);
}

#[tokio::test]
async fn impossible_identifier_budget_is_rejected_before_send_side_effects() -> Result<()> {
    let f = Fixture::new().await?;
    for checkpointed in [false, true] {
        let mut args = f.request.args.clone();
        let mut context = f.send_context();
        if checkpointed {
            context.checkpoint = Some(json!({"session_id": "x".repeat(1000)}));
        } else {
            args["session_id"] = json!("x".repeat(1000));
        }
        args["wait"] = json!({"seconds": 0, "read": {"max_output_bytes": 1024}});
        let error = send(&f.toolset, args, CancellationToken::new(), context)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("metadata"));
    }
    assert!(f.log.last_entry_async().await?.is_none());
    assert!(f.js.get_stream("WORK_NOTIFY_observe").await.is_err());
    Ok(())
}

#[tokio::test]
async fn oversized_diagnostic_identity_cannot_discard_handle_or_observation_metadata() -> Result<()>
{
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let handle = PromptHandle {
        session_id: "child".into(),
        prompt_id: "original-prompt".into(),
    };
    let budget = Budget::new(0, CancellationToken::new());
    let mut scope = match Scope::resolve(&f.toolset, &handle, &f.wait_context(), &budget).await {
        Ok(scope) => scope,
        Err(_) => panic!("scope resolution"),
    };
    scope.admission.invocation_id =
        crate::nats_session_metadata::InvocationIdentity::from_string("x".repeat(2000));
    let mut frame = Frame::new(&handle, 1024);
    frame.preflight(None)?;
    frame.bind(&scope);
    frame.stopped(Stop::Unavailable);
    let result = frame.finish(&budget);
    assert_eq!(result["prompt_id"], "original-prompt");
    assert!(result["invocation_id"].is_null());
    assert!(result["output"]["observation"]["running_ms"].is_number());
    assert!(serialized(&result) <= 1024);
    Ok(())
}
