//! Session- and call-scoped tool cancellation.
//!
//! A cancel is addressed by `(session_id, call_id)` rather than by
//! execution-control generation ownership: whichever process holds the
//! session's log can request one, and the acknowledgement reports whether the
//! tool server accepted it, the call had already finished, it was rejected,
//! or the server cannot tell (retry with the same `cancellation_id`).

use serde::{Deserialize, Serialize};

pub const TOOL_PROTOCOL_VERSION: u32 = 5;

/// Whether a cancel request was accepted, and if not, why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelAcceptance {
    Accepted,
    AlreadyFinished,
    Rejected {
        reason: String,
    },
    /// The cancel may have committed. Retry with the same cancellation ID.
    Unknown {
        reason: String,
    },
}

/// Reply to one [`ControlMessage`], addressed back by session and call id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellationAcknowledgement {
    pub protocol_version: u32,
    pub session_id: String,
    pub call_id: String,
    pub cancellation_id: String,
    pub acceptance: CancelAcceptance,
}

/// Request to cancel one tool invocation, addressed by session and call id.
/// The server name prevents another subscriber on a shared control subject
/// from answering on its behalf.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlMessage {
    pub protocol_version: u32,
    pub server: String,
    pub session_id: String,
    pub operation_id: String,
    pub cancellation_id: String,
    pub call_id: String,
    pub kind: ControlKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Cancel,
}

impl ControlMessage {
    pub fn cancel(
        server: String,
        session_id: String,
        call_id: String,
        cancellation_id: String,
    ) -> Self {
        Self {
            protocol_version: TOOL_PROTOCOL_VERSION,
            server,
            session_id,
            operation_id: call_id.clone(),
            cancellation_id,
            call_id,
            kind: ControlKind::Cancel,
        }
    }

    pub fn acknowledgement(&self, acceptance: CancelAcceptance) -> CancellationAcknowledgement {
        CancellationAcknowledgement {
            protocol_version: TOOL_PROTOCOL_VERSION,
            session_id: self.session_id.clone(),
            call_id: self.call_id.clone(),
            cancellation_id: self.cancellation_id.clone(),
            acceptance,
        }
    }
}
