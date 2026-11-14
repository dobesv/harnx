//! Live state follows the shared turn; output still starts at the joined prompt.
use super::*;
use std::time::Duration;

fn approval_request() -> SessionLogEntry {
    SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "approval-tool".into(),
        summary: "approve?".into(),
        fence_token: 0,
    }
}

fn assert_joined_output(value: &Value, start: u64) {
    let output = value.get("output").unwrap_or(value);
    let events = output["events"].as_array().expect("events");
    assert!(!events.is_empty());
    assert_eq!(events[0]["index"], start);
    assert!(events
        .iter()
        .all(|event| event["index"].as_u64().expect("index") >= start));
    let output = output.to_string();
    assert!(output.contains("joined steering content"), "{output}");
    assert!(!output.contains("original\""), "{output}");
    assert!(!output.contains("tool_calls"), "{output}");
}

async fn assert_joined_blocked(f: &Fixture, start: u64) -> Result<()> {
    let loaded = load_status(f, "join-call").await?;
    let waited = wait_as(f, "join-call", 1, &parent_b_context()).await?;
    assert_eq!(
        (&loaded["status"], &waited["status"]),
        (&json!("blocked"), &json!("blocked"))
    );
    let forever = tokio::time::timeout(
        Duration::from_secs(5),
        wait(
            &f.toolset,
            json!({"session_id": "child", "prompt_id": "join-call", "forever": true}),
            CancellationToken::new(),
            parent_b_context(),
        ),
    )
    .await
    .context("forever wait parked on a blocked joined turn")??;
    assert_eq!(loaded["terminal"], false);
    assert_joined_output(&loaded, start);
    for result in [waited, forever] {
        assert_eq!(result["status"], "blocked");
        assert_eq!(result["terminal"], false);
        assert_eq!(result["wait_expired"], false);
        assert_joined_output(&result, start);
    }
    Ok(())
}

#[tokio::test]
async fn joined_prompt_inherits_approval_requested_before_join() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    block(&f).await?;
    let joined = joining_send(&f, "join-call", "joined steering content").await?;
    assert_joined_blocked(&f, joined.prompt.user_msg_seq()).await?;
    let loaded = load_status(&f, "join-call").await?;
    assert!(!loaded.to_string().contains("hitl_approval_requested"));
    Ok(())
}

#[tokio::test]
async fn joined_prompt_inherits_tool_calls_when_approval_arrives_after_join() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    f.append(tool_calls("approval-tool", "danger")).await?;
    let joined = joining_send(&f, "join-call", "joined steering content").await?;
    let budget = Budget::new(WaitLimit::Seconds(120), BudgetStops::default());
    let scope = Scope::resolve(&f.toolset, &joined.handle, &parent_b_context(), &budget)
        .await
        .map_err(|_| anyhow::anyhow!("scope resolution"))?;
    let mut snapshot = Snapshot::default();
    snapshot.refresh(&f.log, &scope, &budget).await.unwrap();
    assert_eq!(snapshot.status(), SessionPromptStatus::Pending);
    f.append(approval_request()).await?;
    assert_joined_blocked(&f, joined.prompt.user_msg_seq()).await?;
    snapshot.refresh(&f.log, &scope, &budget).await.unwrap();
    assert_eq!(snapshot.status(), SessionPromptStatus::Blocked);
    Ok(())
}

#[tokio::test]
async fn joined_prompt_does_not_inherit_rounds_from_finished_turns() -> Result<()> {
    for terminal in [
        end(1),
        SessionLogEntry::Error {
            message: "old failure".into(),
            fence_token: 0,
            timestamp: None,
        },
        SessionLogEntry::Cancel {
            fence_token: 0,
            cancellation_id: None,
            requested_by: None,
            timestamp: None,
        },
    ] {
        let f = Fixture::new().await?;
        f.dispatch().await?;
        block(&f).await?;
        f.append(terminal).await?;
        let active = joining_send(&f, "active-call", "active original").await?;
        let joined = joining_send(&f, "join-call", "joined steering content").await?;
        assert_eq!(joined.prompt.execution_id(), active.prompt.execution_id());
        let loaded = load_status(&f, "join-call").await?;
        let waited = wait_as(&f, "join-call", 1, &parent_b_context()).await?;
        assert_eq!(loaded["status"], "pending");
        assert_eq!(waited["status"], "pending");
        assert_joined_output(&loaded, joined.prompt.user_msg_seq());
        assert_joined_output(&waited, joined.prompt.user_msg_seq());
        // No old ToolCalls may make an orphan fragment in this turn look valid.
        f.append(approval_request()).await?;
        assert_eq!(load_status(&f, "join-call").await?["status"], "unknown");
        let unknown = wait_as(&f, "join-call", 1, &parent_b_context()).await?;
        assert_unprovable(&unknown, true);
    }
    Ok(())
}

#[tokio::test]
async fn joined_prompt_does_not_inherit_uncertainty_from_finished_turn() -> Result<()> {
    let f = Fixture::new().await?;
    f.dispatch().await?;
    f.append(SessionLogEntry::Clear).await?;
    f.append(end(1)).await?;
    joining_send(&f, "active-call", "active original").await?;
    joining_send(&f, "join-call", "joined steering content").await?;
    assert_eq!(load_status(&f, "join-call").await?["status"], "pending");
    let waited = wait_as(&f, "join-call", 1, &parent_b_context()).await?;
    assert_eq!(waited["status"], "pending");
    assert_eq!(
        waited["output"]["observation"]["error"],
        "soft_wait_expired"
    );
    Ok(())
}
