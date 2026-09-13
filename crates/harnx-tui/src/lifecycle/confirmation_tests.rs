use super::nats_tool_confirmation_handler;
use crate::tool_confirmation::ToolConfirmationReply;
use crate::types::{ToolConfirmationEvent, TuiEvent};
use harnx_core::tool::ToolCall;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn in_process_confirmation_bridge_supports_current_thread_runtime() {
    check_in_process_confirmation(tokio::runtime::Builder::new_current_thread()).await;
}

#[tokio::test]
async fn in_process_confirmation_bridge_supports_multi_thread_runtime() {
    check_in_process_confirmation(tokio::runtime::Builder::new_multi_thread()).await;
}

async fn check_in_process_confirmation(mut runtime_builder: tokio::runtime::Builder) {
    let mut harness = crate::test_utils::TuiTestHarness::new().await;
    let tui = harness.tui();
    tui.install_tool_confirm_bridge();
    for answer in [Some(true), Some(false), None] {
        let confirm = tui.config.read().tui_confirm_tool_use.clone().unwrap();
        let runtime = runtime_builder.enable_all().build().unwrap();
        // The synchronous tool callback has its own thread; the TUI must be
        // free to process the modal while that thread waits for human input.
        let decision = std::thread::spawn(move || {
            runtime.block_on(async move {
                confirm(
                    &ToolCall::new("atlas_session_handoff".into(), json!({}), None, None),
                    &json!({}),
                    None,
                )
            })
        });
        let event = tokio::time::timeout(Duration::from_secs(5), tui.event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let TuiEvent::ToolConfirmation(ToolConfirmationEvent::Show { reply, .. }) = event else {
            panic!("expected tool confirmation event");
        };
        if let Some(answer) = answer {
            reply.send(answer).unwrap();
        } else {
            drop(reply);
        }
        assert_eq!(
            matches!(
                decision.join().unwrap(),
                harnx_runtime::tool::ToolUseConfirmation::Approve
            ),
            answer.unwrap_or(false),
        );
    }
}

#[tokio::test(start_paused = true)]
async fn nats_confirmation_handler_accepts_approval_after_a_day() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = nats_tool_confirmation_handler(event_tx);
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
        tool_name,
        input_preview,
        reason,
        reply,
    }) = event
    else {
        panic!("expected tool confirmation event");
    };
    assert_eq!(tool_name, "atlas_session_handoff");
    assert_eq!(input_preview, r#"{"prompt":"execute"}"#);
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
    let handler = nats_tool_confirmation_handler(event_tx);
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
    let ToolConfirmationReply::Async(reply) = reply else {
        panic!("NATS confirmation must use an async reply");
    };
    assert!(reply.is_closed());
}
