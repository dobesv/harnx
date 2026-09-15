//! Physical cleanup evidence. Logical acceptance never depends on this status.
use crate::CleanupState;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "CleanupStatusWire")]
pub struct CleanupStatus {
    pub state: CleanupState,
    /// The resource owner has finished its per-invocation work, not merely closed a reply.
    pub owner_stopped: bool,
    /// Direct registered physical descendants still awaiting confirmation.
    pub remaining: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Default for CleanupStatus {
    fn default() -> Self {
        Self {
            state: CleanupState::Pending,
            owner_stopped: false,
            remaining: 0,
            last_error: None,
        }
    }
}

impl CleanupStatus {
    pub fn unconfirmed(reason: impl Into<String>) -> Self {
        Self {
            state: CleanupState::Unconfirmed,
            last_error: Some(reason.into()),
            ..Self::default()
        }
    }

    pub fn confirmed() -> Self {
        Self {
            state: CleanupState::Confirmed,
            owner_stopped: true,
            remaining: 0,
            last_error: None,
        }
    }
}

impl crate::Operation {
    pub fn cleanup_status(&self) -> CleanupStatus {
        CleanupStatus {
            state: self.cleanup_state(),
            owner_stopped: self.owner_stopped && !self.abandoned,
            remaining: self.children.len(),
            last_error: self.blocker.clone(),
        }
    }
}

// Existing gate records stored only CleanupState. A legacy label without owner
// evidence is not confirmation; the reconciler must recover physical evidence.
#[derive(Deserialize)]
#[serde(untagged)]
enum CleanupStatusWire {
    Details {
        state: CleanupState,
        owner_stopped: bool,
        remaining: usize,
        last_error: Option<String>,
    },
    Legacy(CleanupState),
}

impl From<CleanupStatusWire> for CleanupStatus {
    fn from(wire: CleanupStatusWire) -> Self {
        match wire {
            CleanupStatusWire::Details {
                state,
                owner_stopped,
                remaining,
                last_error,
            } => Self {
                state,
                owner_stopped,
                remaining,
                last_error,
            },
            CleanupStatusWire::Legacy(CleanupState::Pending) => Self::default(),
            CleanupStatusWire::Legacy(_) => {
                Self::unconfirmed("legacy cleanup label has no owner evidence")
            }
        }
    }
}
