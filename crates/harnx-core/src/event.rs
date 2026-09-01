//! Canonical event stream emitted during an agent turn. Every front-end
//! (TUI, one-shot CLI, HTTP/SSE server, and future MCP/A2A servers)
//! consumes this type via an `AgentEventSink`. See the spec at
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md` for the
//! full rationale; the quick version is that these events are the single
//! source of truth for what happened during a turn, and each front-end
//! reconstructs its protocol's types from event fields instead of passing
//! raw protocol structs through the bus.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api_types::CompletionTokenUsage;

// --- top-level ---------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEvent {
    Model(ModelEvent),
    Tool(ToolEvent),
    Turn(TurnEvent),
    Session(SessionEvent),
    Notice(NoticeEvent),
    User(UserEvent),
    Status(StatusLine),
    Plan {
        entries: Vec<PlanEntry>,
    },
    SubAgent {
        source: AgentSource,
        event: Box<AgentEvent>,
    },
}

impl AgentEvent {
    /// Wraps an event from a sub-agent, preserving the innermost existing source.
    pub fn sub_agent(source: AgentSource, event: AgentEvent) -> AgentEvent {
        let mut source = source;
        let mut event = event;
        while let AgentEvent::SubAgent {
            source: existing,
            event: inner,
        } = event
        {
            source = existing;
            event = *inner;
        }
        AgentEvent::SubAgent {
            source,
            event: Box::new(event),
        }
    }
}

// --- sub-enums ---------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModelEvent {
    MessageChunk {
        blocks: Vec<ContentBlock>,
    },
    ThoughtChunk {
        blocks: Vec<ContentBlock>,
    },
    Final {
        output: String,
        usage: CompletionTokenUsage,
    },
    Error(String),
    Usage {
        input: u64,
        output: u64,
        cached: u64,
        cache_write: u64,
        session_label: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolEvent {
    Started {
        id: String,
        name: String,
        kind: ToolKind,
        markdown: Option<String>,
        input: serde_json::Value,
        locations: Vec<ToolLocation>,
    },
    Progress {
        id: String,
        text: String,
    },
    Update {
        id: String,
        markdown: Option<String>,
        status: Option<ToolStatus>,
        content: Option<Vec<ContentBlock>>,
    },
    Completed {
        id: String,
        output: serde_json::Value,
        /// Pre-rendered display text for the result. `Some(text)` when an
        /// MCP `result_template` (or per-tool config override) has been
        /// rendered against `output`; `None` when no template applied,
        /// in which case consumers fall back to extracting text from
        /// `output` themselves. Mirrors `markdown` on `Started`/`Update`.
        markdown: Option<String>,
    },
    Failed {
        id: String,
        error: String,
    },
    Blocked {
        id: String,
        name: String,
        input: serde_json::Value,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TurnEvent {
    Started,
    RetryAttempt {
        attempt: u32,
        reason: String,
    },
    ModelFallback {
        from: String,
        to: String,
    },
    HandoffRequested {
        agent: String,
        session_id: Option<String>,
    },
    SubAgentStarted {
        agent: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        invocation_id: Option<String>,
    },
    SubAgentProgress(SubAgentProgress),
    Ended {
        outcome: TurnOutcome,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    /// Emitted after a handoff prompt has been durably appended to the target
    /// session and its worker activation has been published.
    HandoffCommitted {
        agent: String,
        session_id: String,
    },
    Saved {
        path: PathBuf,
    },
    CompactingStarted,
    CompactingCompleted,
    CompactingFailed(String),
    AgentInitializing {
        agent: String,
    },
    ModelChanged {
        from: String,
        to: String,
    },
    RagIndexing {
        url: String,
        index: usize,
        total: usize,
    },
    Generic {
        text: String,
    },
    /// Emitted after a message or tool-calls entry has been written to the
    /// session log. The TUI uses this to patch the `seq` field on the
    /// most-recently-pushed `AssistantText` or `ToolCall` transcript item.
    /// NATS clients suppress worker-emitted events and regenerate logical seq
    /// from JetStream history, so this event is still load-bearing for live
    /// TUI mutation commands (edit/delete/rewind) — do not remove.
    LogSeqAssigned {
        seq: usize,
    },
    /// Emitted after a session title has been generated or manually set.
    /// Carries the new title text.
    TitleUpdated(String),
    /// Emitted when automatic session-title generation fails. Carries the
    /// full error chain for display.
    TitleGenerationFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NoticeEvent {
    Info(String),
    Warning(String),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserEvent {
    Message { content: String },
}

// --- supporting types --------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    Image {
        data: Vec<u8>,
        mime: String,
    },
    ResourceLink {
        uri: String,
        name: Option<String>,
    },
    /// Forward-compat passthrough for protocol content kinds this crate
    /// doesn't model directly. The `kind` is the originating protocol's
    /// discriminator; `value` is the original structured payload. Protocol adapters
    /// use this to round-trip unknown content variants without loss.
    Opaque {
        kind: String,
        value: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolLocation {
    pub path: PathBuf,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusLine {
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSource {
    pub agent: String,
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
}

/// Per-delegation progress shared by every frontend.
///
/// A child session can be prompted more than once, so `invocation_id` rather
/// than session identity is the correlation key. Usage and tool counts cover
/// only work performed directly by this invocation; nested agents report their
/// own snapshots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubAgentProgress {
    pub invocation_id: String,
    pub agent: String,
    pub session_id: String,
    pub status: SubAgentProgressStatus,
    pub elapsed_ms: u64,
    pub usage: CompletionTokenUsage,
    pub tool_call_count: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentProgressStatus {
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanEntry {
    pub status: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub output: String,
    pub thought: Option<String>,
    pub usage: CompletionTokenUsage,
    pub handoff: Option<AgentHandoff>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHandoff {
    pub agent: String,
    pub session_id: Option<String>,
}

// --- sink trait --------------------------------------------------------------

/// Every crate that emits agent-visible events does so through an
/// `AgentEventSink`. Implementations are typically built by each front-end
/// and installed on the `SessionCtx`. The trait is object-safe so the
/// sink can be held as `Arc<dyn AgentEventSink>`.
pub trait AgentEventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}

/// A no-op sink useful for tests or code paths that run before a real sink
/// has been installed. Prefer a real sink when possible.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl AgentEventSink for NullSink {
    fn emit(&self, _event: AgentEvent) {}
}

impl AgentSource {
    pub fn heading(&self) -> String {
        let mut parts = vec![self.agent.as_str()];
        if let Some(model) = &self.model {
            if !model.is_empty() {
                parts.push(model);
            }
        }
        if let Some(session_id) = &self.session_id {
            if !session_id.is_empty() {
                parts.push(session_id);
            }
        }
        format!("> {}", parts.join(" ▸ "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_constructs_and_serializes() {
        let event = AgentEvent::Notice(NoticeEvent::Warning("hello".into()));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("Warning"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn sub_agent_progress_round_trips_with_stable_status_names() {
        let event = AgentEvent::Turn(TurnEvent::SubAgentProgress(SubAgentProgress {
            invocation_id: "invocation-1".into(),
            agent: "researcher".into(),
            session_id: "session-1".into(),
            status: SubAgentProgressStatus::Running,
            elapsed_ms: 12_345,
            usage: CompletionTokenUsage::new(Some(11), Some(7), Some(3)),
            tool_call_count: 2,
        }));

        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value["Turn"]["SubAgentProgress"]["status"],
            serde_json::json!("running")
        );
        let decoded: AgentEvent = serde_json::from_value(value).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::Turn(TurnEvent::SubAgentProgress(SubAgentProgress {
                invocation_id,
                elapsed_ms: 12_345,
                tool_call_count: 2,
                ..
            })) if invocation_id == "invocation-1"
        ));
    }

    #[test]
    fn sub_agent_started_accepts_legacy_payload_without_invocation_id() {
        let event: TurnEvent = serde_json::from_value(serde_json::json!({
            "SubAgentStarted": {
                "agent": "researcher",
                "session_id": "session-1"
            }
        }))
        .unwrap();

        assert!(matches!(
            event,
            TurnEvent::SubAgentStarted {
                invocation_id: None,
                ..
            }
        ));
    }

    #[test]
    fn sub_agent_wraps_bare_event() {
        let source = AgentSource {
            agent: "argus".into(),
            session_id: Some("session-1".into()),
            model: None,
        };

        match AgentEvent::sub_agent(
            source.clone(),
            AgentEvent::Model(ModelEvent::Error("failed".into())),
        ) {
            AgentEvent::SubAgent {
                source: actual,
                event,
            } => {
                assert_eq!(actual, source);
                assert!(matches!(*event, AgentEvent::Model(ModelEvent::Error(_))));
            }
            other => panic!("expected sub-agent event, got {other:?}"),
        }
    }

    #[test]
    fn sub_agent_flattens_repeated_wrapping_preserving_innermost_source() {
        let inner_source = AgentSource {
            agent: "inner".into(),
            session_id: None,
            model: None,
        };
        let outer_source = AgentSource {
            agent: "outer".into(),
            session_id: Some("session-2".into()),
            model: Some("model".into()),
        };
        let model_event = AgentEvent::Model(ModelEvent::Error("failed".into()));
        let event = AgentEvent::sub_agent(
            outer_source,
            AgentEvent::sub_agent(inner_source.clone(), model_event),
        );

        match event {
            AgentEvent::SubAgent { source, event } => {
                assert_eq!(source, inner_source);
                assert!(matches!(*event, AgentEvent::Model(ModelEvent::Error(_))));
            }
            other => panic!("expected flattened sub-agent event, got {other:?}"),
        }
    }

    #[test]
    fn sub_agent_preserves_analyst_source_when_researcher_wraps_final() {
        let analyst_source = AgentSource {
            agent: "analyst".into(),
            session_id: Some("analyst-session".into()),
            model: None,
        };
        let researcher_source = AgentSource {
            agent: "researcher".into(),
            session_id: Some("researcher-session".into()),
            model: None,
        };
        let event = AgentEvent::sub_agent(
            researcher_source,
            AgentEvent::sub_agent(
                analyst_source.clone(),
                AgentEvent::Model(ModelEvent::Final {
                    output: "Analyst complete".into(),
                    usage: CompletionTokenUsage::default(),
                }),
            ),
        );

        match event {
            AgentEvent::SubAgent { source, event } => {
                assert_eq!(source, analyst_source);
                assert!(matches!(
                    *event,
                    AgentEvent::Model(ModelEvent::Final { .. })
                ));
            }
            other => panic!("expected flattened analyst event, got {other:?}"),
        }
    }

    #[test]
    fn null_sink_accepts_events() {
        let sink: Box<dyn AgentEventSink> = Box::new(NullSink);
        sink.emit(AgentEvent::Turn(TurnEvent::Started));
    }

    #[test]
    fn tool_kind_default_is_other() {
        assert!(matches!(ToolKind::default(), ToolKind::Other));
    }

    #[test]
    fn content_block_opaque_round_trips() {
        let value = serde_json::json!({"custom": "payload"});
        let block = ContentBlock::Opaque {
            kind: "custom_kind".into(),
            value: value.clone(),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        match back {
            ContentBlock::Opaque { kind, value: v } => {
                assert_eq!(kind, "custom_kind");
                assert_eq!(v, value);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn agent_source_round_trips_through_serde() {
        let source = AgentSource {
            agent: "argus".to_string(),
            session_id: Some("session-1".to_string()),
            model: None,
        };
        let json = serde_json::to_string(&source).unwrap();
        let decoded: AgentSource = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.agent, "argus");
        assert_eq!(decoded.session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn user_event_round_trips_through_serde() {
        let event = AgentEvent::User(UserEvent::Message {
            content: "hello from user".into(),
        });
        let json = serde_json::to_string(&event).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
        match decoded {
            AgentEvent::User(UserEvent::Message { content }) => {
                assert_eq!(content, "hello from user");
            }
            other => panic!("wrong variant after round-trip: {other:?}"),
        }
    }
}
