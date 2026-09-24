//! Safe ACP fallback for committed harnx session handoffs.
//!
//! ACP v1 cannot switch an IDE to an agent-created session. A committed target
//! therefore becomes user-visible text, while the source ACP session stops
//! accepting prompts. Phase 7 can replace this fallback with target following.

use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, SessionUpdate, TextContent};
use harnx_core::agent_ref::AgentRef;
use harnx_core::event::{AgentEvent, SessionEvent};

/// Durable identity of an independently running handoff target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandoffTarget {
    cluster: String,
    agent: String,
    local_session_id: String,
}

impl HandoffTarget {
    /// Resolve committed event identity. Bare agent names inherit source cluster.
    pub fn from_committed(
        committed_agent: &str,
        local_session_id: &str,
        source_cluster: &str,
    ) -> Option<Self> {
        let local_session_id = non_empty(local_session_id)?;
        let (agent, cluster) = match AgentRef::parse(committed_agent) {
            AgentRef::Local(agent) => (non_empty(&agent)?, non_empty(source_cluster)?),
            AgentRef::Remote { agent, cluster } => (non_empty(&agent)?, non_empty(&cluster)?),
        };
        Some(Self {
            cluster,
            agent,
            local_session_id,
        })
    }

    /// Cluster containing target session.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    /// Target agent name without cluster suffix.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    /// Session ID local to target agent and cluster.
    pub fn local_session_id(&self) -> &str {
        &self.local_session_id
    }

    /// Agent selector accepted by TUI and Web surfaces.
    pub fn agent_selector(&self) -> String {
        if self.cluster == harnx_runtime::config::LOCAL_CLUSTER_KEY {
            self.agent.clone()
        } else {
            format!("{}@{}", self.agent, self.cluster)
        }
    }

    /// Message shown when ACP cannot follow committed target automatically.
    pub fn fallback_message(&self) -> String {
        format!(
            "Handoff committed to {}. The target is running independently. \
             ACP clients do not auto-follow handoffs yet. {}",
            self.identity_text(),
            self.open_instructions()
        )
    }

    /// Actionable error returned for prompts sent to handed-off source.
    pub fn prompt_rejection(&self) -> String {
        format!(
            "This ACP session is no longer active because it handed off to {}. \
             The new prompt was not sent to the source session. The target is running \
             independently. {}",
            self.identity_text(),
            self.open_instructions()
        )
    }

    fn identity_text(&self) -> String {
        if self.cluster == harnx_runtime::config::LOCAL_CLUSTER_KEY {
            format!(
                "agent `{}`, local session `{}`",
                self.agent, self.local_session_id
            )
        } else {
            format!(
                "agent `{}`, local session `{}`, cluster `{}`",
                self.agent, self.local_session_id, self.cluster
            )
        }
    }

    fn open_instructions(&self) -> String {
        let selector = self.agent_selector();
        format!(
            "Open it in the TUI with `.session {selector} {}`. For Web, start \
             `harnx-serve --addr 127.0.0.1:8000`, open `http://127.0.0.1:8000/`, \
             and select agent `{selector}`, session `{}`.",
            self.local_session_id, self.local_session_id
        )
    }
}

/// Extract only a top-level authoritative handoff commit.
pub fn committed_target(event: &AgentEvent, source_cluster: &str) -> Option<HandoffTarget> {
    let AgentEvent::Session(SessionEvent::HandoffCommitted {
        agent, session_id, ..
    }) = event
    else {
        return None;
    };
    HandoffTarget::from_committed(agent, session_id, source_cluster)
}

/// Map committed target to ordinary ACP text because ACP v1 has no switch RPC.
pub fn fallback_update(target: &HandoffTarget) -> SessionUpdate {
    let text = TextContent::new(target.fallback_message());
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(text)))
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_target_message_has_identity_and_open_instructions() {
        let target = HandoffTarget::from_committed("atlas@prod", "target-1", "source")
            .expect("valid target");

        assert_eq!(
            (&target.cluster, &target.agent, &target.local_session_id),
            (
                &"prod".to_string(),
                &"atlas".to_string(),
                &"target-1".to_string()
            )
        );
        let message = target.fallback_message();
        for expected in [
            "agent `atlas`",
            "local session `target-1`",
            "cluster `prod`",
            "target is running independently",
            ".session atlas@prod target-1",
            "http://127.0.0.1:8000/",
        ] {
            assert!(
                message.contains(expected),
                "missing `{expected}`: {message}"
            );
        }
    }

    #[test]
    fn local_target_inherits_cluster_without_showing_internal_local_key() {
        let target = HandoffTarget::from_committed(
            "atlas",
            "target-1",
            harnx_runtime::config::LOCAL_CLUSTER_KEY,
        )
        .expect("valid target");

        assert_eq!(target.agent_selector(), "atlas");
        assert!(!target.fallback_message().contains("__local__"));
        assert!(target
            .fallback_message()
            .contains(".session atlas target-1"));
    }

    #[test]
    fn malformed_committed_identity_is_ignored() {
        assert!(HandoffTarget::from_committed("", "target-1", "prod").is_none());
        assert!(HandoffTarget::from_committed("atlas", "", "prod").is_none());
        assert!(HandoffTarget::from_committed("atlas@", "target-1", "prod").is_none());
    }
}
