use super::{fixture::*, *};
use harnx_core::{message::MessageRole, session::SessionLogEntry};

#[tokio::test]
async fn pending_wait_repairs_once_with_fresh_exact_sequence_and_replay_is_idempotent() -> Result<()>
{
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    let saved = f
        .toolset
        .session_metadata
        .prompt_admission(f.log.storage_key(), "original-prompt")
        .await?;
    // A non-originating observer may repair delivery, but cannot replace
    // the admission used by the worker to freeze execution limits.
    let first = wait(
        &f.toolset,
        f.wait_args(json!({"seconds": 1})),
        CancellationToken::new(),
        unrelated_context(),
    )
    .await?;
    assert_eq!(first["status"], "pending");
    assert_eq!(first["dispatch"], "confirmed");
    assert_eq!(f.activation_count().await?, 2);
    let stream = f.js.get_stream("WORK_NOTIFY_observe").await?;
    let raw = stream.get_raw_message(2).await?;
    let activation: crate::nats_worker::SessionActivate = serde_json::from_slice(&raw.payload)?;
    assert_eq!(
        activation.requested_seq,
        Some(original.prompt.user_msg_seq())
    );
    assert_eq!(activation.execution_id.as_deref(), Some("original-prompt"));
    assert!(activation.epoch.starts_with("observe:wait-call:"));
    assert!(activation.tool_confirmation_subject.is_none());
    assert!(activation.token_budget.is_none());
    wait(
        &f.toolset,
        f.wait_args(json!({"seconds": 1})),
        CancellationToken::new(),
        unrelated_context(),
    )
    .await?;
    assert_eq!(
        f.activation_count().await?,
        2,
        "lost-reply replay of same wait deduplicates"
    );
    let later = ToolInvocationContext {
        call_id: "different-wait".into(),
        ..f.wait_context()
    };
    wait(
        &f.toolset,
        f.wait_args(json!({"seconds": 1})),
        CancellationToken::new(),
        later,
    )
    .await?;
    assert_eq!(
        f.activation_count().await?,
        3,
        "new wait has fresh delivery attempt"
    );
    assert_eq!(f.log.load_events_latest_async().await?.len(), 1);
    assert_eq!(
        f.toolset
            .session_metadata
            .prompt_admission(f.log.storage_key(), "original-prompt")
            .await?,
        saved
    );
    Ok(())
}

#[tokio::test]
async fn expired_admission_without_terminal_is_pending_not_timed_out_and_never_repaired(
) -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let a = f
        .toolset
        .session_metadata
        .prompt_admission(f.log.storage_key(), "original-prompt")
        .await?
        .unwrap();
    let mut frozen = a.parent.unwrap();
    frozen.invocation_id = a.invocation_id;
    frozen.deadline = Some(chrono::Utc::now() - chrono::Duration::hours(1));
    f.toolset
        .session_metadata
        .put_invocation_limits(f.log.storage_key(), &frozen)
        .await?;
    let result = wait(
        &f.toolset,
        f.wait_args(json!({"seconds": 1})),
        CancellationToken::new(),
        unrelated_context(),
    )
    .await?;
    assert_eq!(result["status"], "pending");
    assert_eq!(result["terminal"], false);
    assert_eq!(f.activation_count().await?, 1);
    assert_eq!(f.log.load_events_latest_async().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn approval_committed_after_snapshot_suppresses_repair_on_refresh() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    let budget = Budget::new(WaitLimit::Seconds(120), BudgetStops::default());
    let handle = PromptHandle {
        session_id: "child".into(),
        prompt_id: "original-prompt".into(),
    };
    let scope = match Scope::resolve(&f.toolset, &handle, &f.wait_context(), &budget).await {
        Ok(s) => s,
        Err(_) => panic!("scope"),
    };
    let mut snapshot = Snapshot::default();
    snapshot.refresh(&f.log, &scope, &budget).await.unwrap();
    assert_eq!(snapshot.status(), SessionPromptStatus::Pending);
    block(&f).await?;
    let result = super::super::repair::repair_once(&f.toolset, &scope, &mut snapshot, &budget)
        .await
        .unwrap();
    assert_eq!(result, None);
    assert_eq!(snapshot.status(), SessionPromptStatus::Blocked);
    assert_eq!(f.activation_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn activation_repair_failure_keeps_partial_output_and_recoverable_handle() -> Result<()> {
    use async_nats::jetstream::stream::DiscardPolicy;
    let f = Fixture::new().await?;
    f.dispatch().await?;
    f.append(message(
        "partial",
        MessageRole::Assistant,
        "durable partial output",
    ))
    .await?;
    let mut stream = f.js.get_stream("WORK_NOTIFY_observe").await?;
    let mut config = stream.info().await?.config.clone();
    config.max_messages = 1;
    config.discard = DiscardPolicy::New;
    f.js.update_stream(config).await?;
    let result = f.poll(json!({"seconds": 1})).await?;
    assert_eq!(result["prompt_id"], "original-prompt");
    assert_eq!(result["status"], "pending");
    assert_eq!(result["dispatch"], "unconfirmed");
    assert!(result["output"]
        .to_string()
        .contains("durable partial output"));
    assert_eq!(f.log.load_events_latest_async().await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn lifecycle_scan_walks_bounded_pages_and_does_not_use_presentation_filters_or_latest_tail(
) -> Result<()> {
    let f = Fixture::new().await?;
    let original = f.dispatch().await?;
    for _ in 0..1002 {
        f.append(SessionLogEntry::DataUrls {
            urls: Default::default(),
        })
        .await?;
    }
    f.append(message(
        "answer",
        MessageRole::Assistant,
        "answer after historical page",
    ))
    .await?;
    f.append(end(original.prompt.user_msg_seq())).await?;
    f.append(message("later", MessageRole::User, "must not leak"))
        .await?;
    // This test checks lifecycle paging, not read speed. The first poll makes
    // ~2,000 sequential broker reads (lifecycle scan plus presentation window)
    // and the second ~1,000, which can take over a second on slower CI runners
    // (Windows). A timed-out poll keeps its terminal status but drops the page,
    // so give it plenty of room; it returns as soon as the page is ready.
    let result = f.poll(json!({"seconds": 30, "read": {"index_min": 1, "index_max": 1004, "types": ["message"]}})).await?;
    assert_eq!(result["status"], "completed");
    // Lifecycle crossed the physical page cap; presentation still pages rather
    // than loading the entire history to satisfy a sparse filter.
    let next = result["output"]["next_index"]
        .as_u64()
        .context("bounded presentation must expose progress")?;
    let page = f
        .poll(json!({"seconds": 30, "read": {"index_min": next, "types": ["message"]}}))
        .await?;
    assert_eq!(page["status"], "completed");
    assert!(page["output"]
        .to_string()
        .contains("answer after historical page"));
    assert!(!page["output"].to_string().contains("must not leak"));
    assert!(!result["output"].to_string().contains("must not leak"));
    assert!(result["output"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["index"].as_u64().unwrap() >= original.prompt.user_msg_seq()));
    Ok(())
}

fn unrelated_context() -> ToolInvocationContext {
    ToolInvocationContext {
        call_id: "wait-call".into(),
        invoking_session_id: Some("foreign-parent".into()),
        run_context: Some(crate::nats_session::test_support::inherited_tool_context()),
        ..Default::default()
    }
}
