//! Acknowledged dispatch from a completed source turn to a target actor.
//!
//! The handoff preparation callback runs before the tool result is persisted.
//! It resolves the target agent and reserves a child session ID when the caller
//! omits one. Provider results that bypass the callback are prepared at dispatch.
//! Preparation carries provenance (Requested vs Reserved) outside tool output, so
//! dispatch skips caller-ownership validation for reserved IDs. Tool servers
//! cannot forge this provenance; only runtime reservation produces Reserved IDs.

use super::{
    base_event, registry::get_or_spawn_in, test_log, PendingPrompt, PromptResult,
    ResolvedAgentTarget, RunFinished, SessionActor, SessionCommand, SessionHandle, SessionKey,
    SessionMap, SessionPromptOptions, SessionState,
};
use ag_ui_core::event::{Event, RunErrorEvent};
use anyhow::{anyhow, bail, Result};
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
use harnx_runtime::config::{Config, GlobalConfig};
use harnx_runtime::tool::{PrepareHandoffFn, PreparedHandoff};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

const HANDOFF_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Kept outside tool output: a reserved short ID may also exist under another agent.
#[derive(Clone, Debug)]
enum HandoffSessionId {
    Requested(String),
    Reserved(String),
}

impl HandoffSessionId {
    fn as_str(&self) -> &str {
        match self {
            Self::Requested(id) | Self::Reserved(id) => id,
        }
    }

    fn into_string(self) -> String {
        match self {
            Self::Requested(id) | Self::Reserved(id) => id,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct PreparedTarget {
    target: ResolvedAgentTarget,
    session_id: HandoffSessionId,
}

/// One turn's callback state, retained beside LoopResult until actor dispatch.
#[derive(Clone)]
pub(super) struct HandoffPreparation {
    config: Config,
    registry: SessionMap,
    targets: Arc<Mutex<HashMap<String, PreparedTarget>>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl HandoffPreparation {
    pub(super) fn new(config: Config, registry: SessionMap) -> Self {
        Self {
            config,
            registry,
            targets: Default::default(),
            failure: Default::default(),
        }
    }

    pub(super) fn callback(&self) -> Arc<PrepareHandoffFn> {
        let preparation = self.clone();
        Arc::new(move |agent, session_id, tool_call_id| {
            let preparation = preparation.clone();
            Box::pin(async move {
                let result = async {
                    let target = preparation.prepare_target(&agent, session_id).await?;
                    let prepared = PreparedHandoff {
                        agent: target.target.display_ref_with_default_cluster(
                            preparation.config.default_cluster_for_display(),
                        ),
                        session_id: target.session_id.as_str().to_string(),
                    };
                    preparation
                        .targets
                        .lock()
                        .expect("handoff preparation poisoned")
                        .insert(tool_call_id, target);
                    Ok(prepared)
                }
                .await;
                if let Err(error) = &result {
                    // Engine adds fatal-tool context. Keep serve's existing handoff
                    // error text for callers while retaining fatal termination.
                    *preparation
                        .failure
                        .lock()
                        .expect("handoff preparation poisoned") = Some(format!("{error:#}"));
                }
                result
            })
        })
    }

    pub(super) fn take_failure(&self) -> Option<String> {
        self.failure
            .lock()
            .expect("handoff preparation poisoned")
            .take()
    }

    pub(super) async fn take(
        &self,
        tool_call_id: Option<&str>,
        agent: &str,
        session_id: Option<String>,
    ) -> Result<PreparedTarget> {
        let prepared = tool_call_id.and_then(|id| {
            self.targets
                .lock()
                .expect("handoff preparation poisoned")
                .remove(id)
        });
        match prepared {
            Some(target) => Ok(target),
            // Provider switch_agent results don't run the synthetic-tool callback.
            None => self.prepare_target(agent, session_id).await,
        }
    }

    async fn prepare_target(
        &self,
        agent: &str,
        session_id: Option<String>,
    ) -> Result<PreparedTarget> {
        let (target, scoped) = crate::resolve_agent_target(&self.config, agent).await?;
        let session_id = self
            .resolve_session_id(&target, &scoped, session_id)
            .await?;
        Ok(PreparedTarget { target, session_id })
    }

    async fn resolve_session_id(
        &self,
        target: &ResolvedAgentTarget,
        scoped: &GlobalConfig,
        session_id: Option<String>,
    ) -> Result<HandoffSessionId> {
        let Some(session_id) = session_id.filter(|id| !id.trim().is_empty()) else {
            return Config::reserve_new_session_id(scoped)
                .await
                .map(HandoffSessionId::Reserved);
        };
        let owner = self
            .registry
            .iter()
            .find(|entry| {
                entry.key().cluster() == target.cluster()
                    && entry.key().session == session_id
                    && !entry.value().tx.is_closed()
            })
            .map(|entry| entry.key().agent().to_string())
            .or_else(|| {
                (target.cluster() == harnx_runtime::config::LOCAL_CLUSTER_KEY)
                    .then(|| test_log::test_session_owner(&session_id))
                    .flatten()
            });
        match owner {
            Some(owner) if owner != target.agent() => bail!(
                "handoff failed: session '{session_id}' belongs to agent '{owner}', not '{}'",
                target.display_ref_with_default_cluster(self.config.default_cluster_for_display())
            ),
            _ => Ok(HandoffSessionId::Requested(session_id)),
        }
    }
}

pub(super) struct HandoffRequest {
    pub(super) prepared: PreparedTarget,
    pub(super) prompt: String,
    pub(super) handoff_tool_call_id: Option<String>,
}

impl SessionActor {
    pub(super) async fn dispatch_handoff_to_target(
        &mut self,
        done: &RunFinished,
        request: HandoffRequest,
    ) {
        let Some(parent) = done.limits.clone() else {
            self.fail_handoff(done, anyhow!("handoff has no inherited execution context"));
            return;
        };
        let HandoffRequest {
            prepared,
            prompt,
            handoff_tool_call_id,
        } = request;
        let PreparedTarget { target, session_id } = prepared;
        // Requested IDs were validated during preparation. Reserved IDs must never
        // enter caller ownership validation, even if another agent uses the same ID.
        let target_session_id = session_id.into_string();
        let target_key = SessionKey::new(target.clone(), target_session_id.clone());
        if target_key == self.key {
            self.queue_self_handoff(prompt, parent);
        } else if let Err(error) = self.send_handoff_prompt(&target_key, prompt, parent).await {
            self.fail_handoff(done, error);
            return;
        }
        // Use display_ref for the handoff event - this shows the agent name
        // as the user would see it in the UI. When in cluster-client mode,
        // agents on the default cluster are shown without @cluster suffix.
        let display_agent = target.display_ref_with_default_cluster(
            self.actor_config.base_config.default_cluster_for_display(),
        );
        self.commit_handoff(done, display_agent, target_session_id, handoff_tool_call_id);
    }

    fn queue_self_handoff(
        &mut self,
        prompt: String,
        parent: harnx_runtime::nats_session_metadata::RunLimitsRecord,
    ) {
        self.pending.push_back(PendingPrompt {
            text: prompt,
            options: SessionPromptOptions {
                runtime_parent: Some(Box::new(parent)),
                ..Default::default()
            },
        });
    }

    async fn send_handoff_prompt(
        &mut self,
        target_key: &SessionKey,
        prompt: String,
        parent: harnx_runtime::nats_session_metadata::RunLimitsRecord,
    ) -> Result<()> {
        let target_handle = self.get_or_spawn_target_session_actor(target_key.clone());
        let (reply_tx, reply_rx) = oneshot::channel();
        target_handle
            .tx
            .send(SessionCommand::Prompt {
                text: prompt,
                options: SessionPromptOptions {
                    runtime_parent: Some(Box::new(parent)),
                    web_base_url: self.web_address.inferred_base().map(str::to_string),
                    ..Default::default()
                },
                reply: reply_tx,
            })
            .await
            .map_err(|_| target_actor_error(target_key, "dispatch"))?;
        wait_for_handoff_ack(reply_rx, target_key, HANDOFF_ACK_TIMEOUT).await
    }

    fn commit_handoff(
        &mut self,
        done: &RunFinished,
        agent: String,
        session_id: String,
        handoff_tool_call_id: Option<String>,
    ) {
        done.sink
            .emit(AgentEvent::Session(SessionEvent::HandoffCommitted {
                agent,
                session_id,
                handoff_tool_call_id,
                after_seq: None,
            }));
        self.finish_run(done, None);
        self.state = SessionState::Idle;
    }

    pub(super) fn fail_handoff(&mut self, done: &RunFinished, error: anyhow::Error) {
        done.sink.sink.close_text_segment();
        let _ = self.broadcast_tx.send(Event::RunError(RunErrorEvent {
            base: base_event(),
            message: format!("{error:#}"),
            code: None,
        }));
        self.state = SessionState::Idle;
    }

    fn get_or_spawn_target_session_actor(&mut self, target_key: SessionKey) -> SessionHandle {
        get_or_spawn_in(
            &self.registry,
            target_key,
            self.reap_ttl,
            &self.actor_config,
        )
    }
}

fn target_actor_error(target: &SessionKey, phase: &str) -> anyhow::Error {
    anyhow!(
        "handoff failed: target actor for '{}/{}' stopped before {phase}",
        target.agent(),
        target.session
    )
}

async fn wait_for_handoff_ack(
    reply_rx: oneshot::Receiver<PromptResult>,
    target: &SessionKey,
    wait: Duration,
) -> Result<()> {
    match tokio::time::timeout(wait, reply_rx).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err(target_actor_error(target, "acknowledging the prompt")),
        Err(_) => Err(anyhow!(
            "handoff failed: target actor for '{}/{}' did not acknowledge the prompt within {} seconds",
            target.agent(),
            target.session,
            wait.as_secs()
        )),
    }
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
