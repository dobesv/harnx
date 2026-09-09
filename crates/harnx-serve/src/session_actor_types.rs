use ag_ui_core::types::ids::RunId;
use ag_ui_core::{event::Event, types::message::Message as AgUiMessage};
use chrono::{DateTime, Utc};
use harnx_core::abort::AbortSignal;
use tokio::sync::{broadcast, mpsc, oneshot};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SessionKey {
    pub agent: String,
    pub session: String,
}

#[derive(Clone)]
pub struct SessionHandle {
    pub tx: mpsc::Sender<SessionCommand>,
    /// Identifies which actor incarnation this handle talks to. A registry entry can be
    /// replaced by a fresh actor for the same key, and the outgoing actor must only remove
    /// its own entry — never its replacement's.
    ///
    /// Holding a clone of a handle also keeps its actor alive: the idle reap only fires while
    /// the registry's own sender is the last one, so a long-lived clone pins the actor.
    pub(crate) actor_id: u64,
}

pub(crate) struct ActiveRun {
    pub(crate) run_id: RunId,
    #[allow(dead_code)]
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) abort_signal: AbortSignal,
    pub(crate) inject_tx: Option<mpsc::Sender<String>>,
}

pub(crate) struct PendingPrompt {
    pub(crate) text: String,
    pub(crate) options: SessionPromptOptions,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPromptOptions {
    pub working_dir: Option<std::path::PathBuf>,
    pub attachment_refs: Vec<String>,
}

pub enum SessionCommand {
    Subscribe {
        reply: oneshot::Sender<SubscribeResult>,
    },
    Prompt {
        text: String,
        options: SessionPromptOptions,
        reply: oneshot::Sender<PromptResult>,
    },
    Cancel {
        reply: oneshot::Sender<()>,
    },
    HitlApprovalDecision {
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Get {
        reply: oneshot::Sender<SessionInfo>,
    },
    Unsubscribe,
    #[cfg(test)]
    EmitTestEvent {
        event: Event,
    },
    #[cfg(test)]
    Panic,
}

pub struct SubscribeResult {
    pub snapshot: Vec<AgUiMessage>,
    pub history_warnings: Vec<String>,
    pub state: SessionState,
    pub events: broadcast::Receiver<Event>,
    /// Durable log entries for control-state hydration (promptless attach).
    /// Populated by the session actor when refreshing history from NATS.
    pub log_entries: Option<Vec<(u64, harnx_core::session::SessionLogEntry)>>,
    /// Session tokens usage for augmenting hydrated usage events with context fields.
    /// Captured during `refresh_history_snapshot` from the reconstructed session.
    pub tokens_usage: Option<crate::ag_ui::UsageContextSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptResult {
    Accepted { run_id: String },
    Enqueued { run_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCapabilities {
    pub can_prompt: bool,
    pub can_cancel: bool,
    pub supports_snapshot: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub state: SessionState,
    pub history_snapshot: Vec<AgUiMessage>,
    pub history_warnings: Vec<String>,
    pub capabilities: SessionCapabilities,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionState {
    Idle,
    Running {
        run_id: String,
        started_at: DateTime<Utc>,
    },
    /// Durable HITL gate derived from unmatched approval requests in the session log.
    Interrupted {
        pending: Box<PendingInterrupt>,
    },
}

/// Minimal AG-UI interrupt view. Worker continuation state remains in the durable log.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingInterrupt {
    pub metadata: serde_json::Value,
}
