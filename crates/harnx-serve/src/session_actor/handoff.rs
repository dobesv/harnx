//! Acknowledged dispatch from a completed source turn to a target actor.

use super::{
    base_event, registry::get_or_spawn_in, test_log, PendingPrompt, PromptResult, RunFinished,
    SessionActor, SessionCommand, SessionHandle, SessionKey, SessionPromptOptions, SessionState,
};
use ag_ui_core::event::{Event, RunErrorEvent};
use anyhow::{anyhow, bail, Result};
use harnx_core::event::{AgentEvent, AgentEventSink, SessionEvent};
use std::time::Duration;
use tokio::sync::oneshot;

const HANDOFF_ACK_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct HandoffRequest {
    pub(super) agent: String,
    pub(super) session_id: Option<String>,
    pub(super) prompt: String,
}

impl SessionActor {
    pub(super) async fn dispatch_handoff_to_target(
        &mut self,
        done: &RunFinished,
        request: HandoffRequest,
    ) {
        let HandoffRequest {
            agent,
            session_id,
            prompt,
        } = request;
        let target_session_id = match self.resolve_handoff_session_id(&agent, session_id) {
            Ok(session_id) => session_id,
            Err(error) => {
                self.fail_handoff(done, error);
                return;
            }
        };
        let target_key = SessionKey {
            agent: agent.clone(),
            session: target_session_id.clone(),
        };
        if target_key == self.key {
            self.queue_self_handoff(prompt);
        } else if let Err(error) = self.send_handoff_prompt(&target_key, prompt).await {
            self.fail_handoff(done, error);
            return;
        }
        self.commit_handoff(done, agent, target_session_id);
    }

    fn resolve_handoff_session_id(
        &self,
        agent: &str,
        session_id: Option<String>,
    ) -> Result<String> {
        let Some(session_id) = session_id.filter(|id| !id.trim().is_empty()) else {
            return Ok(harnx_runtime::nats_worker::new_remote_session_id());
        };
        let owner = self
            .registry
            .iter()
            .find(|entry| entry.key().session == session_id && !entry.value().tx.is_closed())
            .map(|entry| entry.key().agent.clone())
            .or_else(|| test_log::test_session_owner(&session_id));
        match owner {
            Some(owner) if owner != agent => bail!(
                "handoff failed: session '{session_id}' belongs to agent '{owner}', not '{agent}'"
            ),
            _ => Ok(session_id),
        }
    }

    fn queue_self_handoff(&mut self, prompt: String) {
        self.pending.push_back(PendingPrompt {
            text: prompt,
            options: SessionPromptOptions::default(),
        });
    }

    async fn send_handoff_prompt(&mut self, target_key: &SessionKey, prompt: String) -> Result<()> {
        let target_handle = self.get_or_spawn_target_session_actor(target_key.clone());
        let (reply_tx, reply_rx) = oneshot::channel();
        target_handle
            .tx
            .send(SessionCommand::Prompt {
                text: prompt,
                options: SessionPromptOptions::default(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| target_actor_error(target_key, "dispatch"))?;
        wait_for_handoff_ack(reply_rx, target_key, HANDOFF_ACK_TIMEOUT).await
    }

    fn commit_handoff(&mut self, done: &RunFinished, agent: String, session_id: String) {
        done.sink
            .emit(AgentEvent::Session(SessionEvent::HandoffCommitted {
                agent,
                session_id,
            }));
        self.finish_run(done, None);
        self.state = SessionState::Idle;
    }

    fn fail_handoff(&mut self, done: &RunFinished, error: anyhow::Error) {
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
        target.agent,
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
            target.agent,
            target.session,
            wait.as_secs()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handoff_ack_wait_is_bounded() {
        let (_reply_tx, reply_rx) = oneshot::channel();
        let target = SessionKey {
            agent: "atlas".into(),
            session: "target-session".into(),
        };

        let error = wait_for_handoff_ack(reply_rx, &target, Duration::ZERO)
            .await
            .expect_err("an unresponsive target must time out");

        let error = error.to_string();
        assert!(error.contains("did not acknowledge the prompt"));
        assert!(error.contains("atlas/target-session"));
    }
}
