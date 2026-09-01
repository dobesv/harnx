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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// Ephemeral Core NATS subject owned by the frontend that activated this
    /// turn. Workers use it only when a `PreToolUse` hook asks the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_confirmation_subject: Option<String>,
}

impl SessionActivate {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            epoch: Utc::now().to_rfc3339(),
            requested_seq: None,
            target_worker_id: None,
            token_budget: None,
            tool_confirmation_subject: None,
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

    pub fn with_tool_confirmation_subject(mut self, subject: Option<&str>) -> Self {
        self.tool_confirmation_subject = subject.map(str::to_string);
        self
    }

    pub fn with_token_budget(mut self, token_budget: Option<u64>) -> Self {
        self.token_budget = token_budget;
        self
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
    fn activation_payload_without_token_budget_round_trips() {
        let legacy = br#"{"session_id":"s1","epoch":"now"}"#;
        let activation: SessionActivate = serde_json::from_slice(legacy).unwrap();
        assert_eq!(activation.requested_seq, None);
        assert_eq!(activation.target_worker_id, None);
        assert_eq!(activation.token_budget, None);
        assert_eq!(activation.tool_confirmation_subject, None);
        assert_eq!(activation.msg_id(), "s1:now");

        let encoded = serde_json::to_value(&activation).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({ "session_id": "s1", "epoch": "now" })
        );
        assert_eq!(
            serde_json::from_value::<SessionActivate>(encoded).unwrap(),
            activation
        );
    }

    #[test]
    fn activation_payload_with_token_budget_round_trips() {
        let activation = SessionActivate::new("s1").with_token_budget(Some(4_096));
        let encoded = serde_json::to_value(&activation).unwrap();
        assert_eq!(encoded["token_budget"], 4_096);
        assert_eq!(
            serde_json::from_value::<SessionActivate>(encoded).unwrap(),
            activation
        );
    }
}
