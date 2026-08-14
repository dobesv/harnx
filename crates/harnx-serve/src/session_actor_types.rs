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
    pub resume: Vec<InterruptResume>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptResume {
    pub interrupt_id: String,
    pub status: InterruptResumeStatus,
    pub payload: InterruptResumePayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterruptResumeStatus {
    Approved,
    Denied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptResumePayload {
    pub approved: bool,
    pub reason: Option<String>,
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
    Get {
        reply: oneshot::Sender<SessionInfo>,
    },
    Unsubscribe,
    #[cfg(test)]
    EmitTestEvent {
        event: Event,
    },
}

pub struct SubscribeResult {
    pub snapshot: Vec<AgUiMessage>,
    pub events: broadcast::Receiver<Event>,
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
    pub capabilities: SessionCapabilities,
}

#[derive(Clone, Debug)]
pub enum SessionState {
    Idle,
    Running {
        run_id: String,
        started_at: DateTime<Utc>,
    },
    Interrupted {
        run_id: String,
        started_at: DateTime<Utc>,
        pending: Box<PendingInterruptBatch>,
    },
}

impl PartialEq for SessionState {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Idle, Self::Idle) => true,
            (
                Self::Running {
                    run_id: left_run_id,
                    started_at: left_started_at,
                },
                Self::Running {
                    run_id: right_run_id,
                    started_at: right_started_at,
                },
            ) => left_run_id == right_run_id && left_started_at == right_started_at,
            (
                Self::Interrupted {
                    run_id: left_run_id,
                    started_at: left_started_at,
                    ..
                },
                Self::Interrupted {
                    run_id: right_run_id,
                    started_at: right_started_at,
                    ..
                },
            ) => left_run_id == right_run_id && left_started_at == right_started_at,
            _ => false,
        }
    }
}

/// Pending interrupt batch for tool approval HITL.
#[derive(Clone, Debug)]
pub struct PendingInterruptBatch {
    pub interrupt_run_id: String,
    pub text: String,
    pub attachment_refs: Vec<String>,
    pub completion_output: String,
    pub completion_thought: Option<String>,
    pub tool_calls: Vec<harnx_core::tool::ToolCall>,
    pub interrupts: Vec<ToolApprovalInterruptEntry>,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ToolApprovalInterruptEntry {
    pub id: String,
    pub tool_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub message: String,
    pub reason: Option<String>,
}
