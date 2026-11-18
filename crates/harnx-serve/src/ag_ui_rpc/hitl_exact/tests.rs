use super::*;
use crate::test_support::TestConfigSandbox;
use harnx_core::session::SessionLogEntry;

mod fixture;
use fixture::Fixture;

#[test]
fn exact_hitl_params_require_complete_generation_not_tool_id_only() {
    assert!(parse_params(Some(json!({"tool_call_id":"call","approved":true}))).is_err());
    for request in [
        json!({"invocation_id":"", "tool_round_seq":2,"request_seq":3,"tool_call_id":"call"}),
        json!({"invocation_id":"inv", "tool_round_seq":2,"request_seq":2,"tool_call_id":"call"}),
        json!({"invocation_id":"inv", "tool_round_seq":2,"request_seq":3}),
    ] {
        assert!(parse_params(Some(json!({"request":request,"approved":true}))).is_err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_hitl_rpc_applies_displayed_request_retries_and_conflicts() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    let fixture = Fixture::new(&sandbox).await;
    assert_eq!(
        fixture
            .answer(&fixture.reference, true, "original audit")
            .await,
        json!({"applied":true,"outcome":"applied"})
    );
    assert_eq!(
        fixture
            .answer(&fixture.reference, true, "original audit")
            .await,
        json!({"applied":true,"outcome":"already_applied","approved":true,"note":"original audit"})
    );
    assert_eq!(
        fixture.answer(&fixture.reference, true, "retry note").await,
        json!({"applied":true,"outcome":"already_applied","approved":true,"note":"original audit"})
    );
    assert_eq!(
        fixture.answer(&fixture.reference, false, "opposite").await,
        json!({"applied":false,"outcome":"conflicting","approved":true})
    );
    let entries = fixture.entries().await;
    let decisions: Vec<_> = entries
        .iter()
        .filter_map(|(_, entry)| match entry {
            SessionLogEntry::HitlApprovalDecision {
                tool_call_id,
                approved,
                note,
                ..
            } => Some((tool_call_id.as_str(), *approved, note.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(
        decisions,
        vec![(fixture::CALL, true, Some("original audit"))]
    );
    assert!(fixture::pending_legacy(&entries, &fixture.reference.invocation_id).is_none());
    fixture.lease.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_hitl_rpc_stale_and_rejected_calls_never_answer_reused_tool_id() {
    let _guard = harnx_runtime::client::TestStateGuard::new(None).await;
    let sandbox = TestConfigSandbox::new();
    let fixture = Fixture::new(&sandbox).await;
    assert_eq!(
        fixture.answer(&fixture.reference, true, "first").await["outcome"],
        "applied"
    );
    let newer = fixture.advance().await;
    assert_eq!(newer.tool_call_id, fixture.reference.tool_call_id);
    assert_ne!(newer.request_seq, fixture.reference.request_seq);
    let before = fixture.entries().await;
    for approved in [true, false] {
        assert_eq!(
            fixture
                .answer(&fixture.reference, approved, "stale row")
                .await,
            json!({"applied":false,"outcome":"stale"})
        );
    }
    let (status, invalid) = fixture
        .rpc(
            "session/hitl_decision_exact",
            json!({
                "tool_call_id":fixture::CALL,"approved":false,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid["error"]["code"], -32602);
    let (_, unknown) = fixture
        .rpc(
            "session/hitl_decision_exact_unsupported",
            json!({
                "request":fixture.reference,"approved":false,
            }),
        )
        .await;
    assert_eq!(unknown["error"]["code"], -32601);
    let after = fixture.entries().await;
    assert_eq!(
        after, before,
        "stale/invalid/unknown calls must append nothing"
    );
    let pending = fixture::pending_legacy(&after, &newer.invocation_id).unwrap();
    assert_eq!(pending, newer);
    assert_eq!(
        fixture.answer(&newer, false, "new generation").await["outcome"],
        "applied"
    );
    assert!(fixture::pending_legacy(&fixture.entries().await, &newer.invocation_id).is_none());
    fixture.lease.release().await.unwrap();
}
