use super::*;
use chrono::TimeZone;
use harnx_core::message::{MessageContent, MessageRole};
use serde_json::json;

fn at(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 3, 22, minute, 0).unwrap()
}

fn user(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn assistant(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role: MessageRole::Assistant,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn turn_end(through_seq: u64, minute: u32) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 1,
        timestamp: Some(at(minute)),
        usage: None,
    }
}

fn error(message: &str, minute: u32) -> SessionLogEntry {
    SessionLogEntry::Error {
        message: message.into(),
        fence_token: 1,
        timestamp: Some(at(minute)),
    }
}

fn cancel(requested_by: Option<String>, minute: u32) -> SessionLogEntry {
    SessionLogEntry::Cancel {
        fence_token: 0,
        cancellation_id: None,
        requested_by,
        timestamp: Some(at(minute)),
    }
}

fn timeout_requester(deadline: DateTime<Utc>) -> String {
    harnx_runtime::TimeoutTerminal {
        scope: harnx_runtime::TimeoutScope::OuterRun,
        deadline,
        run_id: "run-1".into(),
        invocation_id: "invocation-1".into(),
    }
    .message()
}

fn json_of(entries: Vec<SessionLogEntry>) -> Option<serde_json::Value> {
    let entries: Vec<(u64, SessionLogEntry)> = (1..).zip(entries).collect();
    last_turn(&entries).map(|turn| serde_json::to_value(turn).unwrap())
}

#[test]
fn no_turn_has_ended_before_the_first_terminator() {
    assert_eq!(json_of(vec![]), None);
    assert_eq!(json_of(vec![user("go"), assistant("working")]), None);
}

#[test]
fn turn_end_reads_as_completed() {
    assert_eq!(
        json_of(vec![user("go"), assistant("done"), turn_end(1, 42)]),
        Some(json!({ "seq": 3, "ended_at": "2026-11-03T22:42:00Z", "outcome": "completed" }))
    );
}

#[test]
fn error_reads_as_error_with_its_message() {
    assert_eq!(
        json_of(vec![
            user("go"),
            assistant("partial"),
            error("provider returned 500", 30)
        ]),
        Some(json!({
            "seq": 3,
            "ended_at": "2026-11-03T22:30:00Z",
            "outcome": "error",
            "message": "provider returned 500",
        }))
    );
}

#[test]
fn deadline_cancel_reads_as_timed_out() {
    let deadline = at(3) + chrono::Duration::hours(2);
    assert_eq!(
        json_of(vec![
            user("go"),
            cancel(Some(timeout_requester(deadline)), 59)
        ]),
        Some(json!({
            "seq": 2,
            "ended_at": "2026-11-03T22:59:00Z",
            "outcome": "timed_out",
            "deadline": "2026-11-04T00:03:00Z",
        }))
    );
}

#[test]
fn other_cancels_read_as_interrupted() {
    assert_eq!(
        json_of(vec![user("go"), cancel(Some("parent:abc".into()), 10)]),
        Some(json!({
            "seq": 2,
            "ended_at": "2026-11-03T22:10:00Z",
            "outcome": "interrupted",
            "requested_by": "parent:abc",
        }))
    );
    assert_eq!(
        json_of(vec![user("go"), cancel(None, 10)]),
        Some(json!({ "seq": 2, "ended_at": "2026-11-03T22:10:00Z", "outcome": "interrupted" }))
    );
}

#[test]
fn later_turns_replace_earlier_outcomes() {
    // A failed turn followed by one that completed: a client resuming after
    // both sees the completion, and a prompt still waiting after it doesn't
    // hide the turn that ended.
    assert_eq!(
        json_of(vec![
            user("first"),
            error("worker lost", 5),
            user("second"),
            assistant("done"),
            turn_end(3, 9),
            user("third"),
        ]),
        Some(json!({ "seq": 5, "ended_at": "2026-11-03T22:09:00Z", "outcome": "completed" }))
    );
}

#[test]
fn ended_at_is_omitted_when_the_entry_has_no_timestamp() {
    let entries = vec![(
        7,
        SessionLogEntry::TurnEnd {
            through_seq: 6,
            fence_token: 1,
            timestamp: None,
            usage: None,
        },
    )];
    assert_eq!(
        serde_json::to_value(last_turn(&entries).unwrap()).unwrap(),
        json!({ "seq": 7, "outcome": "completed" })
    );
}
