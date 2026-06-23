//! Control plane for cancel over NATS.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// P2.4 Control plane: cancel over NATS
// ---------------------------------------------------------------------------

/// Control command sent over the control subject (`sessions.{id}.control`).
///
/// Clients publish these commands to interact with an active session without
/// going through the durable activation workflow. Control is fire-and-forget;
/// durable state lands in the session log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand {
    /// Cancel the current turn.
    ///
    /// Worker-originated: carries the fence token for tombstone.
    /// The worker appends a Cancel entry BEFORE firing the AbortSignal.
    Cancel,
}

impl ControlCommand {
    /// Serialize the command to JSON bytes for NATS publish.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Deserialize from JSON bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }
}

/// Control subject pattern for a session.
///
/// Format: `sessions.{session_id}.control`
pub fn control_subject(session_id: &str) -> String {
    format!("sessions.{session_id}.control")
}

/// Publish a control command to a session's control subject.
///
/// This is the client-side helper for driving control commands. Workers
/// subscribe to this subject and handle commands when holding the lease.
pub async fn publish_control_command(
    client: &async_nats::Client,
    session_id: &str,
    command: &ControlCommand,
) -> Result<()> {
    let subject = control_subject(session_id);
    let payload = command.to_bytes()?;
    client
        .publish(subject, payload.into())
        .await
        .context("publish control command")?;
    Ok(())
}
