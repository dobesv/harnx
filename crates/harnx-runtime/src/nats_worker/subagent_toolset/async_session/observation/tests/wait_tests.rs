use super::{fixture::*, *};
use crate::nats_worker::subagent_toolset::{SubagentNats, SubagentSessionRoute};
use harnx_core::{message::MessageRole, session::SessionLogEntry};
use std::time::Duration;

#[tokio::test]
async fn live_polls_have_changing_observation_metadata() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let first = f.poll(json!({"seconds": 1})).await?;
    tokio::time::sleep(Duration::from_millis(2)).await;
    let second = f.poll(json!({"seconds": 1})).await?;
    assert_pending(&first, true);
    assert_eq!(first["output"]["observation"]["error"], "soft_wait_expired");
    assert_ne!(
        first["output"]["observation"]["observed_at_ms"],
        second["output"]["observation"]["observed_at_ms"]
    );
    Ok(())
}

#[tokio::test]
async fn initial_send_wait_and_later_wait_share_formatter_and_soft_expiry_never_cancels_child(
) -> Result<()> {
    let f = Fixture::new().await?;
    let mut args = f.request.args.clone();
    args["wait"] = json!({"seconds": 1});
    let first = send(&f.toolset, args, CancellationToken::new(), f.send_context()).await?;
    assert_eq!(first["status"], "pending");
    assert_eq!(first["wait_expired"], true);
    assert!(first["elapsed_ms"].as_u64().unwrap() >= 1000);
    assert!(!f
        .log
        .load_events_latest_async()
        .await?
        .iter()
        .any(|(_, e)| matches!(e, SessionLogEntry::Cancel { .. })));
    let later = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(first["output"]["events"], later["output"]["events"]);
    f.append(message(
        "done",
        MessageRole::Assistant,
        "child finishes later",
    ))
    .await?;
    f.append(end(1)).await?;
    let completed = f.poll(json!({"seconds": 1})).await?;
    let mut replay = f.request.args.clone();
    replay["wait"] = json!({"seconds": 1});
    let initial_replay = send(
        &f.toolset,
        replay,
        CancellationToken::new(),
        f.send_context(),
    )
    .await?;
    assert_eq!(initial_replay["status"], "completed");
    assert_eq!(
        initial_replay["output"]["events"],
        completed["output"]["events"]
    );
    Ok(())
}

#[tokio::test]
async fn observer_cancel_returns_handle_without_child_cancel_or_false_cancelled_status(
) -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let cancel = CancellationToken::new();
    let args = f.wait_args(json!({"seconds": 120}));
    let context = f.wait_context();
    let future = wait(&f.toolset, args, cancel.clone(), context);
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("wait returned before observer cancellation: {result:?}"),
        _ = async {
            loop {
                if f.activation_count().await.unwrap() == 2 { break; }
                tokio::task::yield_now().await;
            }
        } => {}
    }
    cancel.cancel();
    let result = future.await?;
    assert_eq!(result["prompt_id"], "original-prompt");
    assert_eq!(result["terminal"], false);
    assert_ne!(result["status"], "cancelled");
    assert_eq!(
        result["output"]["observation"]["error"],
        "observer_cancelled"
    );
    assert_eq!(f.log.load_events_latest_async().await?.len(), 1);
    f.append(end(1)).await?;
    assert_eq!(f.status().await?["status"], "completed");
    Ok(())
}

#[tokio::test]
async fn forged_parent_missing_scope_wrong_agent_session_token_and_missing_binding_fail_closed(
) -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    for context in [
        ToolInvocationContext::default(),
        ToolInvocationContext {
            invoking_session_id: Some("foreign-parent".into()),
            ..f.wait_context()
        },
        ToolInvocationContext {
            invoking_session: Some(harnx_toolset::SessionRef {
                agent: Some("other".into()),
                session_id: "forged".into(),
            }),
            ..f.wait_context()
        },
    ] {
        assert!(wait(
            &f.toolset,
            f.wait_args(json!({"seconds": 1})),
            CancellationToken::new(),
            context
        )
        .await
        .is_err());
    }
    for (key, value) in [
        ("session_id", "wrong-session"),
        ("prompt_id", "wrong-token"),
        ("__harnx_parent_session_id", "trusted-parent"),
    ] {
        let mut args = f.wait_args(json!({"seconds": 1}));
        args[key] = json!(value);
        assert!(
            wait(&f.toolset, args, CancellationToken::new(), f.wait_context())
                .await
                .is_err()
        );
    }
    let wrong_agent = SubagentToolset::new(
        "foreign",
        SubagentSessionRoute::new("observe", crate::SessionActivationRoute::ClusterShared),
        SubagentNats::new(
            f.js.client().clone(),
            f.js.clone(),
            f.toolset.session_metadata.clone(),
            1,
        ),
    );
    assert!(wait(
        &wrong_agent,
        f.wait_args(json!({"seconds": 1})),
        CancellationToken::new(),
        f.wait_context()
    )
    .await
    .is_err());
    assert_initial_dispatch_unchanged(&f).await?;
    Ok(())
}

#[tokio::test]
async fn bounded_terminal_partial_continuation_and_empty_range_keep_whole_result_under_budget(
) -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let index = f
        .append(message(
            "huge",
            MessageRole::Assistant,
            &"🔥\\\"\n".repeat(30_000),
        ))
        .await?;
    f.append(end(1)).await?;
    let result = f.poll(json!({"seconds": 1, "read": {"max_output_bytes": 1024, "head_lines": 1, "tail_lines": 1}})).await?;
    assert_eq!(result["status"], "completed");
    assert!(serialized(&result) <= 1024);
    assert_eq!(result["output"]["truncated"], true);
    let continuation = f.poll(json!({"seconds": 1, "read": {"max_output_bytes": 1024, "entry_index": index, "byte_offset": 0}})).await?;
    assert!(serialized(&continuation) <= 1024);
    assert!(continuation["output"]["continuation"].is_object());
    let empty = f
        .poll(json!({"seconds": 1, "read": {"index_min": 1000}}))
        .await?;
    for key in [
        "events",
        "effective_index_max",
        "scanned_through",
        "continuation",
        "omitted_ranges",
    ] {
        assert!(
            empty["output"].get(key).is_some(),
            "missing canonical field {key}"
        );
    }
    let outside = f
        .poll(json!({"seconds": 1, "read": {"entry_index": 1000, "byte_offset": 0}}))
        .await?;
    assert_eq!(
        outside["output"]["observation"]["error"],
        "entry_outside_prompt_scope"
    );
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn broker_unavailability_keeps_handle_budget_and_unknown_without_terminal_claim() -> Result<()>
{
    let mut f = Fixture::new().await?;
    f.dispatch().await?;
    // Load only the authorized scope, then stop the real broker. The scanner's
    // first read now fails; this is not a simulated successful empty history.
    let handle = PromptHandle {
        session_id: "child".into(),
        prompt_id: "original-prompt".into(),
    };
    let budget = Budget::new(WaitLimit::Seconds(1), BudgetStops::default());
    let scope = match Scope::resolve(&f.toolset, &handle, &f.wait_context(), &budget).await {
        Ok(s) => s,
        Err(_) => panic!("scope resolution"),
    };
    f.toolset.jetstream.set_timeout(Duration::from_millis(50));
    drop(f.server);
    let mut observer = Observer {
        toolset: &f.toolset,
        scope,
        snapshot: Snapshot::default(),
        wait: ResolvedWait::default(),
        budget,
        repaired: false,
        single_pass: false,
    };
    let mut frame = Frame::new(&handle, 1024);
    frame.bind(&observer.scope);
    let reason = observer.run(&mut frame).await.unwrap_err();
    frame.stopped(reason);
    let result = frame.finish(&observer.budget);
    assert_eq!(result["session_id"], "child");
    assert_eq!(result["prompt_id"], "original-prompt");
    assert_eq!(result["status"], "unknown");
    assert_eq!(result["terminal"], false);
    assert!(serialized(&result) <= 1024);
    Ok(())
}

#[tokio::test]
async fn send_without_wait_returns_dispatch_receipt_without_starting_observation_or_repair(
) -> Result<()> {
    let f = Fixture::new().await?;
    let result = send(
        &f.toolset,
        f.request.args.clone(),
        CancellationToken::new(),
        f.send_context(),
    )
    .await?;
    assert_eq!(result["session_id"], "child");
    assert_eq!(result["prompt_id"], "original-prompt");
    assert_pending(&result, false);
    assert!(result["output"].get("events").is_none());
    assert_initial_dispatch_unchanged(&f).await?;
    Ok(())
}

/// The wait context with a new-message signal that fires after `delay`.
fn context_with_new_message(f: &Fixture, delay: Duration) -> ToolInvocationContext {
    let new_message = CancellationToken::new();
    let signal = new_message.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        signal.cancel();
    });
    ToolInvocationContext {
        new_message,
        ..f.wait_context()
    }
}

#[tokio::test]
async fn new_message_ends_session_wait_with_its_pending_result() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let context = context_with_new_message(&f, Duration::from_millis(200));
    let args = f.wait_args(json!({"seconds": 30}));
    let result = wait(&f.toolset, args, CancellationToken::new(), context).await?;
    assert_pending(&result, false);
    assert_eq!(result["stopped_by"], "new_message");
    assert!(result["elapsed_ms"].as_u64().unwrap() < 20_000, "{result}");
    // An ordinary expiry names no stop, and the child kept running.
    let expired = f.poll(json!({"seconds": 1})).await?;
    assert!(expired.get("stopped_by").is_none(), "{expired}");
    assert_pending(&expired, true);
    Ok(())
}

/// A message already pending when send starts its wait still dispatches the
/// prompt, then returns the dispatched handle at once instead of waiting.
#[tokio::test]
async fn new_message_pending_before_send_wait_returns_the_dispatched_handle() -> Result<()> {
    let f = Fixture::new().await?;
    let context = f.send_context();
    context.new_message.cancel();
    let mut args = f.request.args.clone();
    args["wait"] = json!({"seconds": 30});
    let result = send(&f.toolset, args, CancellationToken::new(), context).await?;
    assert_eq!(result["prompt_id"], "original-prompt");
    assert_pending(&result, false);
    assert_eq!(result["stopped_by"], "new_message");
    assert!(result["elapsed_ms"].as_u64().unwrap() < 20_000, "{result}");
    Ok(())
}
