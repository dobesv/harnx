use crate::replay_entries_to_sink;
use harnx_core::event::{
    AgentEvent, AgentEventSink, ContentBlock, ModelEvent, NoticeEvent, SessionEvent, ToolEvent,
    ToolKind, UserEvent,
};
use harnx_core::message::{MessageContent, MessageRole};
use harnx_core::session::{SessionLogEntry, ToolOutput};
use harnx_core::session_reconstruct::apply_log_mutations_nats;
use harnx_core::tool::ToolCall;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordingSink(Mutex<Vec<AgentEvent>>);

impl AgentEventSink for RecordingSink {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn assert_replay(entries: &[(u64, SessionLogEntry)], expected: Vec<AgentEvent>) {
    let sink = Arc::new(RecordingSink::default());
    replay_entries_to_sink(entries, sink.clone());
    assert_eq!(
        serde_json::to_value(&*sink.0.lock().unwrap()).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

fn message(role: MessageRole, text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: None,
        role,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

fn user_event(text: &str) -> AgentEvent {
    AgentEvent::User(UserEvent::Message {
        content: text.into(),
    })
}

fn final_event(text: &str) -> AgentEvent {
    AgentEvent::Model(ModelEvent::Final {
        output: text.into(),
        usage: Default::default(),
    })
}

fn seq_event(seq: usize) -> AgentEvent {
    AgentEvent::Session(SessionEvent::LogSeqAssigned { seq })
}

#[test]
fn replay_emits_messages_and_tool_events_in_order() {
    let entries = vec![
        (10, message(MessageRole::User, "read README.md")),
        (
            42,
            SessionLogEntry::ToolCalls {
                text: "reading file".into(),
                thought: Some("not part of the existing text mapping".into()),
                calls: vec![ToolCall::new(
                    "read".into(),
                    json!({"path": "README.md"}),
                    Some("read-call".into()),
                    None,
                )],
                timestamp: None,
                fence_token: Some(7),
            },
        ),
        (
            43,
            SessionLogEntry::ToolResults {
                results: vec![ToolOutput {
                    id: Some("read-call".into()),
                    name: "read".into(),
                    output: json!({"text": "# Harnx\n"}),
                    markdown: Some("# Harnx".into()),
                    content: vec![],
                    switch_agent: None,
                }],
                timestamp: None,
            },
        ),
        (99, message(MessageRole::Assistant, "done")),
    ];

    assert_replay(
        &entries,
        vec![
            user_event("read README.md"),
            seq_event(0),
            AgentEvent::Tool(ToolEvent::Started {
                id: "read-call".into(),
                name: "read".into(),
                kind: ToolKind::Other,
                markdown: None,
                input: json!({"path": "README.md"}),
                locations: vec![],
            }),
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("reading file".into())],
            }),
            seq_event(1),
            AgentEvent::Tool(ToolEvent::Completed {
                id: "read-call".into(),
                output: json!({"text": "# Harnx\n"}),
                markdown: Some("# Harnx".into()),
            }),
            final_event("done"),
            seq_event(3),
        ],
    );
}

#[test]
fn replay_control_entries_emit_nothing() {
    let entries: Vec<_> = vec![
        SessionLogEntry::TurnEnd {
            through_seq: 1,
            fence_token: 7,
            timestamp: None,
            usage: None,
        },
        SessionLogEntry::SubAgentStarted {
            agent: "child-agent".into(),
            session_id: "child-session".into(),
            invocation_id: None,
            tool_call_id: None,
            started_at: None,
        },
        SessionLogEntry::HandoffCommitted {
            target_agent: "next-agent".into(),
            target_session_id: "next-session".into(),
            handoff_tool_call_id: None,
        },
        SessionLogEntry::HitlApprovalRequested {
            tool_call_id: "read-call".into(),
            summary: "approve read".into(),
            fence_token: 7,
        },
        SessionLogEntry::HitlApprovalDecision {
            tool_call_id: "read-call".into(),
            approved: true,
            note: None,
            fence_token: 7,
        },
        SessionLogEntry::DataUrls {
            urls: Default::default(),
        },
        SessionLogEntry::Compress {
            prompt: "summary".into(),
        },
        SessionLogEntry::Clear,
        SessionLogEntry::EditEntries {
            from: 1,
            to: 1,
            replacements: vec![],
        },
        SessionLogEntry::Rewind { after_seq: 1 },
        SessionLogEntry::Unknown,
    ]
    .into_iter()
    .enumerate()
    .map(|(seq, entry)| (seq as u64, entry))
    .collect();

    assert_replay(&entries, vec![]);
}

#[test]
fn replay_empty_entries_emit_nothing() {
    assert_replay(&[], vec![]);
}

#[test]
fn replay_keeps_cancellation_and_error_notices() {
    let entries = vec![
        (5, SessionLogEntry::Cancel { fence_token: 7 }),
        (
            6,
            SessionLogEntry::Error {
                message: "worker failed".into(),
                fence_token: 7,
                timestamp: None,
            },
        ),
    ];
    assert_replay(
        &entries,
        vec![
            AgentEvent::Notice(NoticeEvent::Warning("Session cancelled".into())),
            AgentEvent::Notice(NoticeEvent::Error("worker failed".into())),
        ],
    );
}

#[test]
fn replay_keeps_precompaction_text_but_numbers_only_active_window() {
    let entries = vec![
        (10, message(MessageRole::User, "old prompt")),
        (20, message(MessageRole::Assistant, "old reply")),
        (
            30,
            SessionLogEntry::Compress {
                prompt: "summary".into(),
            },
        ),
        (40, message(MessageRole::User, "new prompt")),
        (50, message(MessageRole::Assistant, "new reply")),
    ];
    assert_replay(
        &entries,
        vec![
            user_event("old prompt"),
            final_event("old reply"),
            user_event("new prompt"),
            seq_event(0),
            final_event("new reply"),
            seq_event(1),
        ],
    );
}

#[test]
fn replay_effective_entries_keeps_replacement_order_and_shared_physical_sequences() {
    let entries = vec![
        (1, message(MessageRole::User, "original prompt")),
        (2, message(MessageRole::Assistant, "retained reply")),
        (
            8,
            SessionLogEntry::EditEntries {
                from: 1,
                to: 1,
                replacements: vec![
                    serde_yaml::to_string(&message(MessageRole::User, "replacement prompt"))
                        .unwrap(),
                    serde_yaml::to_string(&message(MessageRole::Assistant, "replacement reply"))
                        .unwrap(),
                ],
            },
        ),
    ];
    let effective = apply_log_mutations_nats(&entries).unwrap();
    assert_eq!(
        effective.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        vec![8, 8, 2]
    );
    assert_replay(
        &effective,
        vec![
            user_event("replacement prompt"),
            seq_event(0),
            final_event("replacement reply"),
            seq_event(1),
            final_event("retained reply"),
            seq_event(2),
        ],
    );
}
