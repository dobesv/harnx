use super::{pending::terminal_from_history, RemoteFollowTerminal};
use harnx_core::{
    message::{MessageContent, MessageRole},
    session::SessionLogEntry,
};

fn user() -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text("input".into()),
        timestamp: None,
        fence_token: None,
    }
}

fn end(through_seq: u64) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 1,
        timestamp: None,
        usage: None,
    }
}

fn error() -> SessionLogEntry {
    SessionLogEntry::Error {
        message: "failed".into(),
        fence_token: 1,
        timestamp: None,
    }
}

#[test]
fn terminal_history_requires_a_prompt_sequence_and_a_covering_entry() {
    assert_eq!(terminal_from_history(&[(1, user()), (2, end(1))], 0), None);
    assert_eq!(terminal_from_history(&[(1, user())], 1), None);
    let queued = vec![(1, user()), (2, user()), (3, end(1))];
    assert_eq!(terminal_from_history(&queued, 2), None);
    assert_eq!(
        terminal_from_history(&queued, 1),
        Some(RemoteFollowTerminal::Finished)
    );
}

#[test]
fn terminal_history_distinguishes_durable_failure_and_cancellation() {
    assert_eq!(
        terminal_from_history(&[(1, user()), (2, error())], 1),
        Some(RemoteFollowTerminal::Error("failed".into()))
    );
    let cancel = SessionLogEntry::cancel_request("stop".into(), "test".into());
    let mut log = vec![(1, user()), (2, cancel)];
    assert_eq!(
        terminal_from_history(&log, 1),
        Some(RemoteFollowTerminal::Finished)
    );
    log.push((3, user()));
    assert_eq!(terminal_from_history(&log, 3), None);
    assert_eq!(terminal_from_history(&[(1, error()), (2, user())], 2), None);
}

#[test]
fn terminal_history_ignores_outcomes_of_later_turns() {
    let log = vec![(1, user()), (2, end(1)), (3, user()), (4, error())];
    assert_eq!(
        terminal_from_history(&log, 1),
        Some(RemoteFollowTerminal::Finished)
    );
    assert_eq!(
        terminal_from_history(&log, 3),
        Some(RemoteFollowTerminal::Error("failed".into()))
    );
    let first_failed = vec![(1, user()), (2, error()), (3, user()), (4, end(3))];
    assert_eq!(
        terminal_from_history(&first_failed, 1),
        Some(RemoteFollowTerminal::Error("failed".into()))
    );
}

#[test]
fn terminal_history_retraction_and_rewind_settle_unclaimed_input() {
    for mutation in [
        SessionLogEntry::EditEntries {
            from: 2,
            to: 2,
            replacements: vec![],
        },
        SessionLogEntry::Rewind { after_seq: 1 },
    ] {
        // Rewind must target a logical row; TurnEnd is omitted from that history.
        let reply = SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("reply".into()),
            timestamp: None,
            fence_token: None,
        };
        let log = vec![(1, reply), (2, user()), (3, mutation)];
        assert_eq!(
            terminal_from_history(&log, 2),
            Some(RemoteFollowTerminal::Finished)
        );
    }
}
