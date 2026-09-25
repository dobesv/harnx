use super::nats_tool_confirmation_handler;
use crate::tool_confirmation::ToolConfirmationReply;
use crate::types::{ToolConfirmationEvent, TuiEvent};
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn nats_confirmation_handler_accepts_approval_after_a_day() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = nats_tool_confirmation_handler(
        event_tx,
        Default::default(),
        (
            "test-agent/session-1".to_string(),
            "test-cluster".to_string(),
        ),
    );
    let decision = tokio::spawn(handler(
        harnx_runtime::nats_tool_confirmation::ToolConfirmationRequest {
            session_id: "session-1".to_string(),
            tool_call_id: Some("call-1".to_string()),
            tool_name: "atlas_session_handoff".to_string(),
            arguments: json!({"prompt": "execute"}),
            reason: Some("Hand off this plan?".to_string()),
        },
    ));

    let event = event_rx.recv().await.expect("confirmation modal event");
    let TuiEvent::ToolConfirmation(ToolConfirmationEvent::Show {
        confirmation_id,
        origin_session_id,
        cluster,
        tool_call_id,
        tool_name,
        arguments,
        reason,
        reply,
    }) = event
    else {
        panic!("expected tool confirmation event");
    };
    assert_eq!(tool_name, "atlas_session_handoff");
    assert_eq!(origin_session_id, "test-agent/session-1");
    assert_eq!(cluster, "test-cluster");
    assert_eq!(tool_call_id, Some("call-1".to_string()));
    assert_eq!(*arguments, json!({"prompt": "execute"}));
    assert_eq!(reason.as_deref(), Some("Hand off this plan?"));
    assert_ne!(confirmation_id, 0);
    tokio::time::advance(std::time::Duration::from_secs(24 * 60 * 60)).await;
    assert!(!decision.is_finished(), "user input must not expire");
    assert!(event_rx.try_recv().is_err(), "modal must remain open");
    reply.send(true).expect("reply to confirmation");
    assert!(decision.await.expect("confirmation task"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_nats_confirmation_requests_modal_dismissal() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = nats_tool_confirmation_handler(
        event_tx,
        Default::default(),
        (
            "test-agent/session-1".to_string(),
            "test-cluster".to_string(),
        ),
    );
    let decision = tokio::spawn(handler(
        harnx_runtime::nats_tool_confirmation::ToolConfirmationRequest {
            session_id: "session-1".to_string(),
            tool_call_id: Some("call-1".to_string()),
            tool_name: "atlas_session_handoff".to_string(),
            arguments: json!({"prompt": "execute"}),
            reason: Some("Hand off this plan?".to_string()),
        },
    ));

    let event = event_rx.recv().await.expect("confirmation modal event");
    let TuiEvent::ToolConfirmation(ToolConfirmationEvent::Show {
        confirmation_id,
        reply,
        ..
    }) = event
    else {
        panic!("expected tool confirmation event");
    };

    decision.abort();
    let _ = decision.await;
    let dismiss = event_rx.recv().await.expect("confirmation dismissal event");
    assert!(matches!(
        dismiss,
        TuiEvent::ToolConfirmation(ToolConfirmationEvent::Dismiss {
            confirmation_id: dismissed
        }) if dismissed == confirmation_id
    ));

    // Cancellation must release the waiter even before the TUI processes
    // the queued dismissal event.
    let ToolConfirmationReply::Routed { reply, .. } = reply;
    assert!(reply.is_closed());
}
