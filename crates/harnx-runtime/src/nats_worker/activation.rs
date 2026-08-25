//! Session activation payloads and client-visible routing.

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Typed route used by session clients when publishing an activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionActivationRoute {
    ClusterShared,
    WorkerTargeted {
        session_scope: String,
        worker_id: String,
    },
}

/// Activation request published by a client to wake or claim a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionActivate {
    pub session_id: String,
    pub epoch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_worker_id: Option<String>,
}

impl SessionActivate {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            epoch: Utc::now().to_rfc3339(),
            requested_seq: None,
            target_worker_id: None,
        }
    }

    pub fn targeted(
        session_id: impl Into<String>,
        requested_seq: u64,
        worker_id: impl Into<String>,
    ) -> Self {
        Self {
            requested_seq: Some(requested_seq),
            target_worker_id: Some(worker_id.into()),
            ..Self::new(session_id)
        }
    }

    /// Dedup id for the cluster-shared notify stream: session plus epoch.
    pub fn msg_id(&self) -> String {
        format!("{}:{}", self.session_id, self.epoch)
    }
}

/// Generate a fresh remote session id (UUID v7, time-ordered).
pub fn new_remote_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Generate a fresh persistent-deployment worker identity.
pub fn new_worker_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_payload_contains_only_session_routing_state() {
        let legacy = br#"{"session_id":"s1","epoch":"now"}"#;
        let activation: SessionActivate = serde_json::from_slice(legacy).unwrap();
        assert_eq!(activation.requested_seq, None);
        assert_eq!(activation.target_worker_id, None);
        assert_eq!(activation.msg_id(), "s1:now");
    }
}
