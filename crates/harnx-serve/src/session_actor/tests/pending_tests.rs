use super::*;
use harnx_core::{
    message::{MessageContent, MessageRole},
    session::SessionLogEntry,
};

fn user() -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("pending".into()),
        timestamp: None,
        fence_token: None,
    }
}

#[test]
fn durable_pending_state_does_not_require_a_lease() {
    assert_eq!(derive_state_from_log(None), SessionState::Idle);
    let mut log = vec![
        (1, user()),
        (2, user()),
        (
            3,
            SessionLogEntry::TurnEnd {
                through_seq: 1,
                fence_token: 1,
                timestamp: None,
                usage: None,
            },
        ),
    ];
    assert_eq!(derive_state_from_log(Some(&log)), SessionState::Pending);
    assert!(!crate::ag_ui_events::session_state_is_active(
        &SessionState::Pending
    ));
    log.push((
        4,
        SessionLogEntry::TurnEnd {
            through_seq: 2,
            fence_token: 1,
            timestamp: None,
            usage: None,
        },
    ));
    assert_eq!(derive_state_from_log(Some(&log)), SessionState::Idle);
}

#[test]
fn durable_terminal_state_settles_pending_input_but_not_later_prompts() {
    for (terminal, expected) in [
        (
            SessionLogEntry::cancel_request("stop".into(), "test".into()),
            SessionState::Interrupted { cancel_seq: 2 },
        ),
        (
            SessionLogEntry::Error {
                message: "failed".into(),
                fence_token: 1,
                timestamp: None,
            },
            SessionState::Idle,
        ),
    ] {
        let mut log = vec![(1, user()), (2, terminal)];
        assert_eq!(derive_state_from_log(Some(&log)), expected);
        log.push((3, user()));
        assert_eq!(derive_state_from_log(Some(&log)), SessionState::Pending);
    }
}

#[test]
fn pending_approval_keeps_its_state_instead_of_generic_running() {
    let log = vec![
        (1, user()),
        (
            2,
            SessionLogEntry::HitlApprovalRequested {
                tool_call_id: "approval".into(),
                summary: "confirm".into(),
                fence_token: 1,
            },
        ),
    ];
    assert!(matches!(
        derive_state_from_log(Some(&log)),
        SessionState::AwaitingApproval { .. }
    ));
}

#[test]
fn check_admitted_turn_result_distinguishes_timeout_from_cancellation() {
    use harnx_runtime::NatsTurnResult;

    // Timeout error yields explicit error
    let timeout_marker = format!(
        "worker deadline: harnx:timeout {}",
        serde_json::to_string(&harnx_runtime::TimeoutTerminal {
            scope: harnx_runtime::TimeoutScope::OuterRun,
            deadline: chrono::Utc::now(),
            run_id: "run-1".into(),
            invocation_id: "inv-1".into(),
        })
        .unwrap()
    );
    let timeout_result = NatsTurnResult {
        response: None,
        session_id: "test".into(),
        was_cancelled: true,
        error: Some(timeout_marker.clone()),
        user_msg_seq: 1,
        user_msg_id: "msg-1".into(),
    };
    let result = super::check_admitted_turn_result(Ok(timeout_result));
    assert!(result.is_err());
    if let Err(err) = result {
        assert!(err.to_string().contains("deadline expired"));
    } else {
        panic!("expected error result");
    }

    // Manual cancellation completes normally
    let cancel_result = NatsTurnResult {
        response: None,
        session_id: "test".into(),
        was_cancelled: true,
        error: Some("user:cancel".into()),
        user_msg_seq: 1,
        user_msg_id: "msg-1".into(),
    };
    let result = super::check_admitted_turn_result(Ok(cancel_result));
    assert!(result.is_ok());

    // Success completes normally
    let success_result = NatsTurnResult {
        response: Some("done".into()),
        session_id: "test".into(),
        was_cancelled: false,
        error: None,
        user_msg_seq: 1,
        user_msg_id: "msg-1".into(),
    };
    let result = super::check_admitted_turn_result(Ok(success_result));
    assert!(result.is_ok());
}
