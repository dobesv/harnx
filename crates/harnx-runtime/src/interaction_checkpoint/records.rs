//! Private write-ahead records. Public log entries contain only opaque keys.
use harnx_core::{interaction::InteractionStep, tool::ToolCall};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptStart {
    pub version: u32,
    pub session_storage_key: String,
    pub tool_round_seq: u64,
    pub tool_call_id: String,
    pub attempt_id: String,
    pub chain_version: String,
    pub checkpoint: Option<String>,
    pub fence_token: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchIntent {
    pub version: u32,
    pub attempt_id: String,
    /// Also the NATS wire call ID. Never the model's correlation ID.
    pub physical_invocation_id: String,
    pub kind: DispatchKind,
    pub step_path: Vec<InteractionStep>,
    pub call: ToolCall,
    pub fence_token: u64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchKind {
    Next,
    CallTool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchCompleted {
    pub version: u32,
    pub physical_invocation_id: String,
    pub snapshot: String,
    pub fence_token: u64,
}

pub fn session_prefix(session: &str) -> String {
    format!("sessions.{}.", harnx_core::crypto::sha256(session))
}

pub fn round_prefix(session: &str, round: u64) -> String {
    format!("{}rounds.{round}.", session_prefix(session))
}

pub fn call_prefix(session: &str, round: u64, call: &str) -> String {
    format!(
        "{}calls.{}.",
        round_prefix(session, round),
        harnx_core::crypto::sha256(call)
    )
}
