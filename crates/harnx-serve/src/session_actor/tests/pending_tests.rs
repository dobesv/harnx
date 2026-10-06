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
