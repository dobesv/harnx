use super::*;

#[tokio::test]
async fn exact_hitl_decision_checks_child_session_access_before_routing() -> Result<()> {
    let f = fixture(true).await?.context("NATS required")?;
    let storage = harnx_core::session_identity::session_key(Some("allowed"), "foreign");
    let log = NatsSessionLog::new(f.jetstream.clone(), storage).with_replicas(1);
    let round = log
        .append_event_async(&SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![harnx_core::tool::ToolCall::new(
                "read".into(),
                json!({}),
                Some("child-call".into()),
                None,
            )],
            timestamp: None,
            fence_token: None,
        })
        .await?;
    let request_seq = log
        .append_event_async(&SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "child-call".into(),
            summary: "Child approval".into(),
            fence_token: 0,
        })
        .await?;
    let request = harnx_runtime::ApprovalRef {
        invocation_id: "child-invocation".into(),
        tool_round_seq: round,
        request_seq,
        tool_call_id: "child-call".into(),
    };
    let before = log.load_events_latest_async().await?;
    let denied = f
        .session("foreign", "alice")
        .request(Method::POST, "")
        .json(
            &json!({"jsonrpc":"2.0","id":1,"method":"session/hitl_decision_exact",
            "params":{"request":request,"approved":true,"note":"unauthorized"}}),
        )
        .send()
        .await?;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    let after = log.load_events_latest_async().await?;
    assert_eq!(after, before, "access rejection must not append a decision");
    let pending = harnx_runtime::interactions::derive_pending_interaction_summary(
        &after,
        "allowed",
        "local",
        "foreign",
        &request.invocation_id,
    )?
    .context("child request must remain pending")?;
    assert_eq!(
        pending.request,
        harnx_core::pending_interaction::PendingInteractionRequest::LegacyApproval { request }
    );
    Ok(())
}
