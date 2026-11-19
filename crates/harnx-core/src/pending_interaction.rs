//! Exact identities and bounded descriptions of user input owned by a child session.

use serde::{Deserialize, Serialize};

/// Durable identity of one legacy approval request. A provider's tool call ID
/// alone isn't unique across rounds or invocations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRef {
    pub invocation_id: String,
    pub tool_round_seq: u64,
    pub request_seq: u64,
    pub tool_call_id: String,
}

/// Maximum UTF-8 message bytes in progress. Bounded tool results may shorten it
/// further to retain the exact request identity within their output budget.
pub const PENDING_INTERACTION_MESSAGE_MAX_BYTES: usize = 512;

/// First request the direct child would display, not a relayed descendant's
/// request. `agent` and the bare local `session_id` address the owning session's
/// endpoint; control subjects use the storage key derived from that pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInteractionSummary {
    pub agent: String,
    /// NATS routing cluster for the owning endpoint, not a broker URL.
    pub cluster: String,
    pub session_id: String,
    #[serde(flatten)]
    pub request: PendingInteractionRequest,
    pub tool_name: String,
    pub message: String,
    /// Further requests waiting behind this one, in the child's display order.
    pub pending_count: usize,
}

/// Identity to send to the owning child's exact-decision API. Interaction
/// `request_seq` is the durable `InteractionRequested` sequence, not the
/// `SuspensionReady` sequence. `interaction_kind` is the request's `harnx.kind`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingInteractionRequest {
    LegacyApproval {
        request: ApprovalRef,
    },
    Interaction {
        interaction_id: String,
        request_seq: u64,
        interaction_kind: String,
    },
}

#[cfg(test)]
#[path = "pending_interaction_tests.rs"]
mod tests;
