use super::*;
use crate::{
    nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig},
    nats_worker::tests::spawn_test_nats,
};
use harnx_core::{session::SessionLogEntry, tool::ToolCall};
use serde_json::json;

#[test]
fn hitl_control_request_seq_is_optional_on_the_wire() {
    let legacy = json!({"type": "hitl_approval_decision", "tool_call_id": "call", "approved": true, "note": null});
    let command: ControlCommand = serde_json::from_value(legacy.clone()).unwrap();
    assert!(matches!(
        command,
        ControlCommand::HitlApprovalDecision {
            request_seq: None,
            ..
        }
    ));
    assert_eq!(serde_json::to_value(command).unwrap(), legacy);
    let command = ControlCommand::HitlApprovalDecision {
        tool_call_id: "call".into(),
        approved: false,
        note: None,
        request_seq: Some(42),
    };
    assert_eq!(
        ControlCommand::from_bytes(&command.to_bytes().unwrap()).unwrap(),
        command
    );
    #[derive(Deserialize)]
    struct LegacyDecision {
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
    }
    let old: LegacyDecision = serde_json::from_slice(&command.to_bytes().unwrap()).unwrap();
    assert_eq!(old.tool_call_id, "call");
    assert!(!old.approved);
    assert_eq!(old.note, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_control_fences_replaced_requests_and_reused_ids_at_append() -> Result<()> {
    harnx_core::require_nextest();
    let (url, _server, _dir) = spawn_test_nats().await.context("nats-server required")?;
    let client = async_nats::connect(url).await?;
    let js = async_nats::jetstream::new(client.clone());
    let session_id = "hitl-control-fence";
    let lease = Arc::new(
        NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id,
            worker_id: "hitl-worker".into(),
            generation: 1,
            config: NatsLeaseConfig::default(),
            session_metadata: None,
        })
        .await?
        .context("worker lease")?,
    );
    let backend = NatsSessionLogBackend::new(js.clone(), session_id, 1);
    let (tx, mut decisions) = tokio::sync::mpsc::unbounded_channel();
    let handler = SessionControlHandler {
        client,
        jetstream: js,
        session_id: session_id.into(),
        lease: lease.clone(),
        backend: backend.clone(),
        abort_signal: crate::utils::create_abort_signal(),
        hitl_decision_tx: tx,
    };
    let calls = SessionLogEntry::ToolCalls {
        text: String::new(),
        thought: None,
        calls: vec![ToolCall::new(
            "read".into(),
            json!({}),
            Some("reused".into()),
            None,
        )],
        timestamp: None,
        fence_token: Some(lease.fence_token()),
    };
    let request = SessionLogEntry::HitlApprovalRequested {
        tool_call_id: "reused".into(),
        summary: "Approve read".into(),
        fence_token: lease.fence_token(),
    };
    backend.append_event(&calls).await?;
    let original = backend.append_event(&request).await?;
    // A replaced request in the SAME round must not adopt the old answer.
    let replacement = backend.append_event(&request).await?;
    handler
        .apply_hitl_decision("reused".into(), true, None, Some(original), None)
        .await;
    let tail = backend.last_entry_async().await?.unwrap().0;
    assert_eq!(tail, replacement);
    assert!(decisions.try_recv().is_err());
    // Only the replacement generation is eligible for the fenced append.
    handler
        .apply_hitl_decision("reused".into(), true, None, Some(replacement), None)
        .await;
    assert!(decisions.try_recv().is_ok());
    backend
        .append_event(&SessionLogEntry::ToolResults {
            results: vec![],
            timestamp: None,
        })
        .await?;
    backend.append_event(&calls).await?;
    let next = backend.append_event(&request).await?;
    handler
        .apply_hitl_decision("reused".into(), false, None, Some(replacement), None)
        .await;
    handler
        .apply_hitl_decision("wrong-call".into(), false, None, Some(next), None)
        .await;
    assert_eq!(backend.last_entry_async().await?.unwrap().0, next);
    assert!(decisions.try_recv().is_err());
    handler
        .apply_hitl_decision(
            "reused".into(),
            false,
            Some("fresh".into()),
            Some(next),
            None,
        )
        .await;
    assert_eq!(decisions.try_recv().unwrap().note.as_deref(), Some("fresh"));
    backend
        .append_event(&SessionLogEntry::ToolResults {
            results: vec![],
            timestamp: None,
        })
        .await?;
    backend.append_event(&calls).await?;
    let pending = backend.append_event(&request).await?;
    let terminal = backend
        .append_event(&SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: lease.fence_token(),
            timestamp: None,
            usage: None,
        })
        .await?;
    handler
        .apply_hitl_decision("reused".into(), true, None, Some(pending), None)
        .await;
    assert_eq!(backend.last_entry_async().await?.unwrap().0, terminal);
    assert!(decisions.try_recv().is_err());
    lease.release().await?;
    Ok(())
}
