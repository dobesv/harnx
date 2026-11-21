//! Steered-drain coverage (issue #2774): steered rows past the root prompt's
//! own terminator stay outstanding until answered, and Cancel/Error that end
//! the invocation cover them so recovery cannot wedge a task.
use super::hitl::{InvocationProgressScope, PromptProgress};
use super::{steered_rows_outstanding, steered_terminal_seq, NatsSession};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{CompactOutcome, SessionLogEntry};

fn user_row(id: Option<&str>, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: id.map(str::to_owned),
        role: MessageRole::User,
        content: MessageContent::Text(text.to_string()),
        timestamp: None,
        fence_token: None,
    }
}

fn turn_end(through_seq: u64) -> SessionLogEntry {
    SessionLogEntry::TurnEnd {
        through_seq,
        fence_token: 1,
        timestamp: None,
        usage: None,
    }
}

fn cancel() -> SessionLogEntry {
    SessionLogEntry::Cancel {
        fence_token: 0,
        cancellation_id: None,
        requested_by: None,
        timestamp: None,
    }
}

fn error(text: &str) -> SessionLogEntry {
    SessionLogEntry::Error {
        message: text.to_string(),
        fence_token: 1,
        timestamp: None,
    }
}

fn compact_result(id: &str) -> SessionLogEntry {
    SessionLogEntry::CompactResult {
        compaction_id: id.to_string(),
        outcome: CompactOutcome::Compacted,
        timestamp: None,
    }
}

fn retract(from: u64, to: u64) -> SessionLogEntry {
    SessionLogEntry::EditEntries {
        from: from.try_into().unwrap(),
        to: to.try_into().unwrap(),
        replacements: vec![],
    }
}

/// Root prompt at seq 1 plus its own terminator: the plain completed turn.
fn base_turn() -> Vec<(u64, SessionLogEntry)> {
    vec![(1, user_row(Some("root"), "question")), (2, turn_end(1))]
}

#[test]
fn steered_row_past_own_turn_end_is_outstanding() {
    let mut entries = base_turn();
    entries.push((3, user_row(Some("steer-1"), "steered")));
    assert!(steered_rows_outstanding(&entries, 1).unwrap());
    // The prompt's own terminator is not the whole story: the invocation
    // extends to the continuation that answers the steered row.
    entries.push((4, user_row(Some("steer-2"), "second steer")));
    assert!(steered_rows_outstanding(&entries, 1).unwrap());
    assert_eq!(steered_terminal_seq(&entries, 1), None);
}

#[test]
fn continuation_turn_end_covers_steered_rows() {
    let mut entries = base_turn();
    entries.push((3, user_row(Some("steer-1"), "steered")));
    entries.push((4, turn_end(3)));
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
    assert_eq!(steered_terminal_seq(&entries, 1), Some(4));

    // The latest covering TurnEnd extends the invocation, even when a
    // continuation answered several steered rows in successive rounds.
    entries.push((5, user_row(Some("steer-2"), "second steer")));
    entries.push((6, turn_end(5)));
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
    assert_eq!(steered_terminal_seq(&entries, 1), Some(6));
}

#[test]
fn retracted_steered_row_is_not_outstanding() {
    let mut entries = base_turn();
    entries.push((3, user_row(Some("steer-1"), "withdrawn")));
    entries.push((4, retract(3, 3)));
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
}

#[test]
fn compaction_copy_of_covered_row_is_history_not_new_work() {
    // C7 regression: an ordinary compacted conversation must never look like
    // a pending steered drain. Compaction re-logs kept rows after the
    // marker under their original ids; a copy is history, not new input.
    let mut entries = base_turn();
    entries.push((3, user_row(Some("steer-1"), "answered")));
    entries.push((4, turn_end(3)));
    entries.push((5, compact_result("c1")));
    entries.push((6, user_row(Some("steer-1"), "answered")));
    entries.push((7, user_row(Some("root"), "question")));
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());

    // A compacted conversation with no steered rows at all stays quiet too.
    let mut plain = base_turn();
    plain.push((3, compact_result("c2")));
    plain.push((4, user_row(Some("root"), "question")));
    assert!(!steered_rows_outstanding(&plain, 1).unwrap());
}

#[test]
fn non_executable_rows_are_not_outstanding() {
    // Runtime notes and timing notices are context, not owed work.
    let note_id = harnx_core::runtime_note::new_runtime_note_message_id();
    let origin: chrono::DateTime<chrono::Utc> = "2024-01-15T12:00:00Z".parse().unwrap();
    let receipt = harnx_core::timing_identity::TimingReceipt::initial(origin, "inv".to_string());
    let notice = format!(
        "{}{}{}Elapsed: 0s, Budget: 1h",
        harnx_core::timing_identity::TIMING_HEADER,
        receipt.to_json(),
        harnx_core::timing_identity::TIMING_CONTEXT_SEPARATOR
    );
    let mut entries = base_turn();
    entries.push((3, user_row(Some(&note_id), "[Runtime note] note")));
    entries.push((4, user_row(Some("timing-1"), &notice)));
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
}

#[test]
fn id_less_executable_row_past_coverage_is_outstanding() {
    // An executable user row without an id cannot be matched to a compaction
    // copy, so it counts as owed work: the worker's admission verification
    // fails closed on exactly these rows.
    let mut entries = base_turn();
    entries.push((3, user_row(None, "steered")));
    assert!(steered_rows_outstanding(&entries, 1).unwrap());
}

#[test]
fn cancel_after_steered_drain_covers_rows_and_extends_the_bound() {
    // Owner lost after a cancelled continuation: the drain gate must release,
    // and the extended terminal bound points at the Cancel so recovery
    // settles with the cancelled outcome instead of archiving the root
    // answer.
    let mut entries = base_turn();
    entries.push((3, user_row(Some("steer-1"), "steered")));
    entries.push((4, cancel()));
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
    assert_eq!(steered_terminal_seq(&entries, 1), Some(4));
}

#[test]
fn error_after_steered_drain_covers_rows_and_extends_the_bound() {
    let mut entries = base_turn();
    entries.push((3, user_row(Some("steer-1"), "steered")));
    entries.push((4, error("provider failed")));
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
    assert_eq!(steered_terminal_seq(&entries, 1), Some(4));
}

#[test]
fn root_abnormal_terminator_needs_no_extension() {
    // A cancel/failure before any TurnEnd ends the invocation at the first
    // terminator: no steered rows can exist past it in this invocation.
    let entries = vec![(1, user_row(Some("root"), "question")), (2, cancel())];
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
    assert_eq!(steered_terminal_seq(&entries, 1), None);

    let entries = vec![(1, user_row(Some("root"), "question")), (2, error("boom"))];
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
    assert_eq!(steered_terminal_seq(&entries, 1), None);
}

#[test]
fn plain_completed_turn_needs_no_extension() {
    let entries = base_turn();
    assert!(!steered_rows_outstanding(&entries, 1).unwrap());
    assert_eq!(steered_terminal_seq(&entries, 1), None);
}

#[test]
fn steered_drain_with_pending_approval_surfaces_input_required() {
    // The root round finished (its TurnEnd covers only the root row) and the
    // steered row opened a continuation round whose tool call awaits HITL
    // approval: the progress oracle must surface InputRequired, not Running
    // or a root-terminal result, so the parked HITL reply path still
    // publishes INPUT_REQUIRED while the drain is outstanding.
    let entries = vec![
        (1, user_row(Some("root"), "question")),
        (2, turn_end(1)),
        (3, user_row(Some("steer-1"), "steered")),
        (
            4,
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![harnx_core::tool::ToolCall::new(
                    "bash_exec".into(),
                    serde_json::json!({"command": "true"}),
                    Some("drain-call".into()),
                    None,
                )],
                timestamp: None,
                fence_token: Some(1),
            },
        ),
        (
            5,
            SessionLogEntry::InteractionRequested {
                tool_round_seq: 4,
                tool_call_id: "drain-call".into(),
                interaction_id: "int-drain".into(),
                producer: "middleware:gate".into(),
                step_path: vec![],
                key: "gate".into(),
                kind: "confirm".into(),
                payload: serde_json::json!({}),
                summary: "Approve the steered call?".into(),
                chain_version: "v1".into(),
                checkpoint: "private".into(),
                fence_token: 1,
            },
        ),
        (
            6,
            SessionLogEntry::SuspensionReady {
                tool_round_seq: 4,
                pending: vec!["int-drain".into()],
                fence_token: 1,
            },
        ),
    ];
    let scope = InvocationProgressScope {
        invocation_id: "inv-drain",
        prompt_id: "root",
        prompt_sequence: 1,
        session_id: "session",
    };
    let progress = NatsSession::invocation_progress_from_entries(&scope, &entries, true).unwrap();
    assert!(
        matches!(progress, PromptProgress::InputRequired(_)),
        "a pending approval during a steered drain must surface InputRequired, got {progress:?}"
    );
    // Without the approval the same drain stays Running, never Terminal on
    // the root answer.
    let drained: Vec<_> = entries.into_iter().take(3).collect();
    let progress = NatsSession::invocation_progress_from_entries(&scope, &drained, true).unwrap();
    assert!(matches!(
        progress,
        PromptProgress::Running {
            last_decision: None
        }
    ));
}
