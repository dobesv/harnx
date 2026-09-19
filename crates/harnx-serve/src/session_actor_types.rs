use ag_ui_core::types::ids::RunId;
use ag_ui_core::{event::Event, types::message::Message as AgUiMessage};
use chrono::{DateTime, Utc};
use harnx_core::abort::AbortSignal;
use harnx_runtime::{config::LOCAL_CLUSTER_KEY, nats_session::InterruptOutcome};
use tokio::sync::{broadcast, mpsc, oneshot};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ResolvedAgentTarget {
    agent: String,
    cluster: String,
}

impl ResolvedAgentTarget {
    pub fn new(agent: impl Into<String>, cluster: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            cluster: cluster.into(),
        }
    }

    pub fn local(agent: impl Into<String>) -> Self {
        Self::new(agent, LOCAL_CLUSTER_KEY)
    }

    pub fn agent(&self) -> &str {
        &self.agent
    }

    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    pub fn display_ref(&self) -> String {
        if self.cluster == LOCAL_CLUSTER_KEY {
            self.agent.clone()
        } else {
            format!("{}@{}", self.agent, self.cluster)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SessionKey {
    target: ResolvedAgentTarget,
    pub session: String,
}

impl SessionKey {
    pub fn new(target: ResolvedAgentTarget, session: impl Into<String>) -> Self {
        Self {
            target,
            session: session.into(),
        }
    }

    pub fn local(agent: impl Into<String>, session: impl Into<String>) -> Self {
        Self::new(ResolvedAgentTarget::local(agent), session)
    }

    pub fn target(&self) -> &ResolvedAgentTarget {
        &self.target
    }

    pub fn agent(&self) -> &str {
        self.target.agent()
    }

    pub fn cluster(&self) -> &str {
        self.target.cluster()
    }

    pub fn display_ref(&self) -> String {
        self.target.display_ref()
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// NATS storage key for this session.
    ///
    /// **Invariant:** The storage key uses only the bare agent name and session ID.
    /// The cluster determines which JetStream namespace/store to use, but is deliberately
    /// excluded from the key itself. Putting the cluster (or `agent@cluster` string) into
    /// the storage key would break CLI/TUI compatibility and mis-route storage.
    /// The cluster *does* participate in `Eq`/`Hash` for actor isolation (local and remote
    /// sessions with the same agent+id are distinct actors), but storage keys are
    /// cluster-scoped by namespace, not key content.
    pub fn storage_key(&self) -> String {
        harnx_core::session_identity::session_key(Some(self.agent()), self.session())
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    pub tx: mpsc::Sender<SessionCommand>,
    /// Identifies which actor incarnation this handle talks to. A registry entry can be
    /// replaced by a fresh actor for the same key, and the outgoing actor must only remove
    /// its own entry — never its replacement's.
    ///
    /// Holding a clone of a handle also keeps its actor alive: the idle reap only fires while
    /// the registry's own sender is the last one, so a long-lived clone pins the actor.
    pub(crate) actor_id: u64,
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
    pub(crate) admitted: Option<harnx_runtime::nats_session::AppendedPrompt>,
    pub working_dir: Option<std::path::PathBuf>,
    pub attachment_refs: Vec<String>,
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
        reply: oneshot::Sender<Result<InterruptOutcome, String>>,
    },
    HitlApprovalDecision {
        tool_call_id: String,
        approved: bool,
        note: Option<String>,
        reply: oneshot::Sender<Result<bool, String>>,
    },
    Get {
        reply: oneshot::Sender<SessionInfo>,
    },
    Unsubscribe,
    #[cfg(test)]
    EmitTestEvent {
        event: Event,
    },
    #[cfg(test)]
    Panic,
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn display_ref_omits_local_cluster_and_includes_remote_cluster() {
        assert_eq!(ResolvedAgentTarget::local("atlas").display_ref(), "atlas");
        assert_eq!(
            ResolvedAgentTarget::new("atlas", "shared").display_ref(),
            "atlas@shared"
        );
    }

    #[test]
    fn cluster_scopes_actor_identity_but_not_storage_key() {
        let local = SessionKey::local("atlas", "review-12345");
        let remote = SessionKey::new(ResolvedAgentTarget::new("atlas", "shared"), "review-12345");

        assert_ne!(local, remote);
        assert_eq!(local.storage_key(), remote.storage_key());
        assert_eq!(
            local.storage_key(),
            harnx_core::session_identity::session_key(Some("atlas"), "review-12345")
        );
    }
}
pub struct SubscribeResult {
    pub snapshot: Vec<AgUiMessage>,
    pub history_warnings: Vec<String>,
    pub state: SessionState,
    pub events: broadcast::Receiver<Event>,
    /// Durable log entries for control-state hydration (promptless attach).
    /// Populated by the session actor when refreshing history from NATS.
    pub log_entries: Option<Vec<(u64, harnx_core::session::SessionLogEntry)>>,
    /// Session tokens usage for augmenting hydrated usage events with context fields.
    /// Captured during `refresh_history_snapshot` from the reconstructed session.
    pub tokens_usage: Option<crate::ag_ui::UsageContextSnapshot>,
    /// Canonical metadata state used to reconstruct a fresher attached history without reloading it.
    pub session_base: Option<harnx_core::session::Session>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptResult {
    Accepted { run_id: String },
    Enqueued { run_id: String },
    Rejected { reason: String },
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
    /// A worker holds this session's lease, so its turn is running somewhere
    /// even when `state` has no run this server started.
    pub worker_active: bool,
    pub history_snapshot: Vec<AgUiMessage>,
    pub history_warnings: Vec<String>,
    pub capabilities: SessionCapabilities,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionState {
    Idle,
    Running {
        run_id: String,
        started_at: DateTime<Utc>,
    },
    /// The interrupt is being appended to the session log, or the append failed
    /// and nobody knows yet whether the turn was stopped.
    Interrupting,
    /// A `Cancel` is in the log at `cancel_seq`; the turn it stopped is over as
    /// far as this session is concerned.
    Interrupted {
        cancel_seq: u64,
    },
    /// Durable HITL gate derived from unmatched approval requests in the session log.
    AwaitingApproval {
        pending: Box<PendingInterrupt>,
    },
}

/// Minimal AG-UI interrupt view. Worker continuation state remains in the durable log.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingInterrupt {
    pub metadata: serde_json::Value,
}
