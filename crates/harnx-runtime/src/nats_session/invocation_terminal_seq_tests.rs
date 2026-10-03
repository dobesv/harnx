//! Historical result consumers must stop at the first terminal, not the session tail.
use super::{invocation_terminal_seq, NatsSession};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{CompactOutcome, SessionLogEntry};

pub(crate) struct Expected {
    pub prompt: u64,
    pub terminal: Option<u64>,
    pub response: Option<String>,
    pub error: Option<String>,
    pub cancelled: bool,
}

pub(crate) struct Case {
    pub name: String,
    pub entries: Vec<(u64, SessionLogEntry)>,
    pub turns: Vec<Expected>,
}

fn message(role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: role.is_assistant().then_some(1),
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

fn cancel(reason: &str) -> SessionLogEntry {
    SessionLogEntry::Cancel {
        fence_token: 0,
        cancellation_id: Some(uuid::Uuid::new_v4().to_string()),
        requested_by: Some(reason.into()),
        timestamp: None,
    }
}

fn error(text: &str) -> SessionLogEntry {
    SessionLogEntry::Error {
        message: text.into(),
        fence_token: 1,
        timestamp: None,
    }
}

fn compact_request(id: &str) -> SessionLogEntry {
    SessionLogEntry::CompactRequest {
        fence_token: 0,
        compaction_id: id.into(),
        requested_by: Some("manual compaction".into()),
        timestamp: None,
    }
}

fn compact_result(id: &str, outcome: CompactOutcome) -> SessionLogEntry {
    SessionLogEntry::CompactResult {
        compaction_id: id.into(),
        outcome,
        timestamp: None,
    }
}

fn transcript(rows: Vec<SessionLogEntry>) -> Vec<(u64, SessionLogEntry)> {
    rows.into_iter()
        .enumerate()
        .map(|(index, mut entry)| {
            let seq = index as u64 + 1;
            if let SessionLogEntry::Message { id, .. } = &mut entry {
                *id = Some(format!("message-{seq}"));
            }
            (seq, entry)
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Boundary {
    TurnEnd,
    Cancel,
    Error,
    Timeout,
    Compact,
    CompactFailed,
}

impl Boundary {
    fn maintenance(self) -> bool {
        matches!(self, Self::Compact | Self::CompactFailed)
    }

    fn cancelled(self) -> bool {
        matches!(self, Self::Cancel | Self::Timeout)
    }

    fn error(self, label: &str) -> Option<String> {
        match self {
            Self::Error => Some(format!("{label} model failed")),
            Self::Timeout => Some(
                crate::TimeoutTerminal {
                    scope: if label == "original" {
                        crate::TimeoutScope::OuterRun
                    } else {
                        crate::TimeoutScope::InheritedDeadline
                    },
                    deadline: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                    run_id: format!("{label}-run"),
                    invocation_id: format!("{label}-invocation"),
                }
                .message(),
            ),
            _ => None,
        }
    }

    fn entry(self, through_seq: u64, label: &str) -> SessionLogEntry {
        match self {
            Self::TurnEnd => turn_end(through_seq),
            Self::Cancel => cancel("user cancel"),
            Self::Error => error(&self.error(label).unwrap()),
            Self::Timeout => cancel(&self.error(label).unwrap()),
            Self::Compact => compact_result(label, CompactOutcome::Compacted),
            Self::CompactFailed => {
                compact_result(label, CompactOutcome::Failed("summary failed".into()))
            }
        }
    }
}

/// Same canonical fixtures feed private extractor tests and broker-backed cancel/result tests.
pub(crate) fn historical_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for first in [
        Boundary::TurnEnd,
        Boundary::Cancel,
        Boundary::Error,
        Boundary::Timeout,
        Boundary::Compact,
        Boundary::CompactFailed,
    ] {
        for has_response in [false, true] {
            // Manual compaction produces a receipt, not an assistant reply.
            if first.maintenance() && has_response {
                continue;
            }
            for later in [
                Boundary::TurnEnd,
                Boundary::Error,
                Boundary::Cancel,
                Boundary::Timeout,
            ] {
                let mut rows = vec![
                    message(MessageRole::User, "prior prompt"),
                    message(MessageRole::Assistant, "prior output"),
                    turn_end(2),
                    if first.maintenance() {
                        compact_request("original")
                    } else {
                        message(MessageRole::User, "original prompt")
                    },
                ];
                if has_response {
                    rows.push(message(MessageRole::Assistant, "original output"));
                }
                let terminal = rows.len() as u64 + 1;
                rows.push(first.entry(terminal - 1, "original"));
                let later_prompt = rows.len() as u64 + 1;
                rows.push(message(MessageRole::User, "later prompt"));
                rows.push(message(MessageRole::Assistant, "later output"));
                let later_terminal = rows.len() as u64 + 1;
                rows.push(later.entry(later_terminal - 1, "later"));
                cases.push(Case {
                    name: format!("{first:?}/response={has_response}/later={later:?}"),
                    entries: transcript(rows),
                    turns: vec![
                        Expected {
                            prompt: 4,
                            terminal: Some(terminal),
                            response: has_response.then(|| "original output".into()),
                            error: first.error("original"),
                            cancelled: first.cancelled(),
                        },
                        Expected {
                            prompt: later_prompt,
                            terminal: Some(later_terminal),
                            response: Some("later output".into()),
                            error: later.error("later"),
                            cancelled: later.cancelled(),
                        },
                    ],
                });
            }
        }
    }
    cases
}

fn assert_consumers(case: &Case) {
    assert!(case.entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
    for expected in &case.turns {
        let context = format!("{} prompt={}", case.name, expected.prompt);
        assert_eq!(
            invocation_terminal_seq(&case.entries, expected.prompt),
            expected.terminal,
            "{context}: terminal"
        );
        assert_eq!(
            NatsSession::extract_final_response(&case.entries, expected.prompt),
            expected.response,
            "{context}: response"
        );
        assert_eq!(
            NatsSession::extract_turn_error(&case.entries, expected.prompt),
            expected.error,
            "{context}: error"
        );
        assert_eq!(
            NatsSession::extract_turn_outcome(&case.entries, expected.prompt),
            (expected.response.clone(), expected.error.clone()),
            "{context}: outcome"
        );
    }
}

#[test]
fn historical_terminal_consumer_matrix() {
    let cases = historical_cases();
    assert_eq!(cases.len(), 40);
    for case in cases {
        // Prove the original result is unchanged by appending each later turn.
        let first = &case.turns[0];
        let prefix = &case.entries[..first.terminal.unwrap() as usize];
        assert_eq!(
            NatsSession::extract_turn_outcome(prefix, first.prompt),
            (first.response.clone(), first.error.clone()),
            "{}: before later turn",
            case.name
        );
        assert_consumers(&case);
    }
}

#[test]
fn historical_terminal_boundary_semantics_matrix() {
    let cases = [
        ("empty", vec![], 1, None, None),
        (
            "prompt missing beyond tail",
            vec![message(MessageRole::User, "old"), turn_end(1)],
            3,
            None,
            None,
        ),
        (
            "unterminated partial reply",
            vec![
                message(MessageRole::User, "pending"),
                message(MessageRole::Assistant, "partial"),
            ],
            1,
            None,
            Some("partial"),
        ),
        // A new user row isn't itself a terminal; these raw consumers don't validate admission.
        (
            "unterminated span includes available tail",
            vec![
                message(MessageRole::User, "pending"),
                message(MessageRole::User, "steering"),
                message(MessageRole::Assistant, "tail"),
            ],
            1,
            None,
            Some("tail"),
        ),
        (
            "through_seq below prompt ignored",
            vec![
                message(MessageRole::User, "first"),
                message(MessageRole::User, "queued"),
                turn_end(1),
                message(MessageRole::Assistant, "queued reply"),
                turn_end(4),
            ],
            2,
            Some(5),
            Some("queued reply"),
        ),
        (
            "through_seq covers steering too",
            vec![
                message(MessageRole::User, "first"),
                message(MessageRole::User, "steering"),
                message(MessageRole::Assistant, "combined reply"),
                turn_end(3),
            ],
            1,
            Some(4),
            Some("combined reply"),
        ),
        (
            "maintenance ignores TurnEnd",
            vec![
                compact_request("manual"),
                turn_end(1),
                compact_result("manual", CompactOutcome::Compacted),
            ],
            1,
            Some(3),
            None,
        ),
        (
            "ordinary prompt ignores CompactResult",
            vec![
                message(MessageRole::User, "pending"),
                compact_request("automatic"),
                compact_result("automatic", CompactOutcome::Compacted),
                message(MessageRole::Assistant, "after compaction"),
                turn_end(4),
            ],
            1,
            Some(5),
            Some("after compaction"),
        ),
        // Orphan/mismatched receipt failure injection, not successful maintenance.
        (
            "maintenance requires matching ID",
            vec![
                compact_request("manual"),
                compact_result("other", CompactOutcome::Compacted),
            ],
            1,
            None,
            None,
        ),
    ];
    for (name, rows, prompt, terminal, response) in cases {
        assert_consumers(&Case {
            name: name.into(),
            entries: transcript(rows),
            turns: vec![Expected {
                prompt,
                terminal,
                response: response.map(str::to_owned),
                error: None,
                cancelled: false,
            }],
        });
    }

    // Missing prompt in a retained tail doesn't make these raw extractors validate admission.
    let mut retained_tail = transcript(vec![
        message(MessageRole::User, "pruned prompt"),
        message(MessageRole::Assistant, "retained output"),
        turn_end(2),
    ]);
    retained_tail.remove(0);
    assert_consumers(&Case {
        name: "missing prompt in retained tail".into(),
        entries: retained_tail,
        turns: vec![Expected {
            prompt: 1,
            terminal: Some(3),
            response: Some("retained output".into()),
            error: None,
            cancelled: false,
        }],
    });
}
