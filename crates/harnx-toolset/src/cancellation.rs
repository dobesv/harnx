//! Protocol v4 separates durable stop acceptance from physical cleanup.
//!
//! The `CancelAcceptance` tagged enum (`Accepted`, `AlreadyFinished`, `Rejected`,
//! `Unknown`) is independent from the optional `CleanupStatus` field. This replaces
//! the v3 `stopped: bool` field which conflated logical acceptance with physical cleanup.
//!
//! Protocol v4 requires atomic deployment: workers and tool-servers must be upgraded
//! together. V3 clients receive a rejection with guidance to "upgrade workers and tool
//! servers together".

use harnx_execution_control::{CleanupStatus, ExecutionContext, OperationRef, StopReceipt};
use serde::{Deserialize, Serialize};

pub const TOOL_PROTOCOL_VERSION: u32 = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelAcceptance {
    Accepted {
        stop: StopReceipt,
    },
    AlreadyFinished,
    Rejected {
        reason: String,
    },
    /// The durable stop may have committed. Retry with the same cancellation ID.
    Unknown {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancellationAcknowledgement {
    pub protocol_version: u32,
    pub generation: OperationRef,
    pub operation_id: String,
    pub cancellation_id: String,
    pub acceptance: CancelAcceptance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<CleanupStatus>,
}

/// Generation-bound, idempotent request to the resource owner. The server name
/// prevents another subscriber on the shared control subject from answering it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlMessage {
    pub protocol_version: u32,
    pub execution: ExecutionContext,
    pub server: String,
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
    pub fn cancel(execution: ExecutionContext, server: String, cancellation_id: String) -> Self {
        Self {
            protocol_version: TOOL_PROTOCOL_VERSION,
            operation_id: execution.operation().execution_id.clone(),
            call_id: execution.operation().execution_id.clone(),
            execution,
            server,
            cancellation_id,
            kind: ControlKind::Cancel,
        }
    }

    pub fn acknowledgement(
        &self,
        acceptance: CancelAcceptance,
        cleanup: Option<CleanupStatus>,
    ) -> CancellationAcknowledgement {
        CancellationAcknowledgement {
            protocol_version: TOOL_PROTOCOL_VERSION,
            generation: self.execution.generation().clone(),
            operation_id: self.operation_id.clone(),
            cancellation_id: self.cancellation_id.clone(),
            acceptance,
            cleanup,
        }
    }
}
