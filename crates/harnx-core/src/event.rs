//! Canonical event stream emitted during an agent turn. Every front-end
//! (TUI, one-shot CLI, HTTP/SSE server, ACP server, future MCP/A2A servers)
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
    Status(StatusLine),
    Plan { entries: Vec<PlanEntry> },
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
        session_label: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolEvent {
    Started {
        id: String,
        name: String,
        kind: ToolKind,
        title: Option<String>,
        input: serde_json::Value,
        locations: Vec<ToolLocation>,
    },
    Progress {
        id: String,
        text: String,
    },
    Update {
        id: String,
        title: Option<String>,
        status: Option<ToolStatus>,
        content: Option<Vec<ContentBlock>>,
    },
    Completed {
        id: String,
        output: serde_json::Value,
        content: Vec<ContentBlock>,
    },
    Failed {
        id: String,
        error: String,
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
    Ended {
        outcome: TurnOutcome,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    Saved {
        path: PathBuf,
    },
    CompactingStarted,
    Autonaming,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NoticeEvent {
    Info(String),
    Warning(String),
    Error(String),
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
    /// discriminator; `value` is the original structured payload. `harnx-acp`
    /// uses this to round-trip unknown ACP content variants without loss.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
