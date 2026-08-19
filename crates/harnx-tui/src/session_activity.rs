use crate::types::{Tui, TuiEvent};
use harnx_core::event::{AgentEvent, SessionEvent, TurnEvent};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::{GlobalConfig, LOCAL_CLUSTER_KEY};
use harnx_runtime::nats_event_sink::SessionEventStream;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
type SessionTarget = (String, String);

impl Tui {
    pub(super) fn draw_with_session_activity<B>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> anyhow::Result<()>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        self.sync_session_activity_monitor();
        terminal.draw(|frame| self.draw(frame))?;
        Ok(())
    }

    pub(super) async fn handle_session_activity(
        &mut self,
        session_id: String,
        cluster: String,
        active: bool,
    ) {
        let target = (session_id, cluster);
        if self.session_activity_target.as_ref() != Some(&target) {
            return;
        }
        if self.current_prompt_abort.is_some() {
            return;
        }
        if self.app.llm_busy == active {
            return;
        }
        if active {
            self.app.llm_busy = true;
            self.active_remote_session = Some(target);
            self.refresh_input_chrome();
        } else {
            self.complete_main_prompt().await;
        }
    }

    pub(super) async fn handle_turn_activity(
        &mut self,
        event: &AgentEvent,
        is_sub_agent: bool,
    ) -> bool {
        if is_sub_agent {
            return false;
        }
        match event {
            AgentEvent::Turn(TurnEvent::Started) => {
                self.app.llm_busy = true;
                self.app.streaming_open = false;
                self.app.main_streamed_text_idx = None;
                self.refresh_input_chrome();
                true
            }
            AgentEvent::Turn(TurnEvent::Ended { .. }) => {
                self.flush_pending_thought();
                self.app.streaming_open = false;
                self.complete_main_prompt().await;
                true
            }
            _ => false,
        }
    }

    pub(super) fn sync_session_activity_monitor(&mut self) {
        let desired = self.session_activity_destination();
        if desired == self.session_activity_target {
            return;
        }

        self.stop_session_activity_monitor();
        self.session_activity_target = desired.clone();
        self.session_activity_handle = desired.map(|target| {
            spawn_session_activity_monitor(self.config.clone(), self.event_tx.clone(), target)
        });
    }

    fn stop_session_activity_monitor(&mut self) {
        if let Some(handle) = self.session_activity_handle.take() {
            handle.abort();
        }
    }

    fn session_activity_destination(&self) -> Option<(String, String)> {
        let config = self.config.read();
        let session_id = config.session.as_ref()?.id().to_string();
        let cluster = config
            .remote_agent
            .as_ref()
            .map(|(_, cluster)| cluster.clone())
            .unwrap_or_else(|| LOCAL_CLUSTER_KEY.to_string());
        Some((session_id, cluster))
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.stop_session_activity_monitor();
    }
}

fn spawn_session_activity_monitor(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    target: SessionTarget,
) -> JoinHandle<()> {
    tokio::spawn(monitor_session_activity(config, event_tx, target))
}

async fn monitor_session_activity(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    target: SessionTarget,
) {
    loop {
        if !monitor_session_connection(&config, &event_tx, &target).await {
            return;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn monitor_session_connection(
    config: &GlobalConfig,
    event_tx: &UnboundedSender<TuiEvent>,
    target: &SessionTarget,
) -> bool {
    let mut stream = match attach_session_event_stream(config, target).await {
        Ok(stream) => stream,
        Err(error) => {
            log::debug!(
                "failed to attach session activity monitor: session_id={} cluster={} error={error:#}",
                target.0,
                target.1,
            );
            return true;
        }
    };
    if !send_activity(event_tx, target, history_has_pending_turn(stream.history())) {
        return false;
    }
    forward_session_activity(&mut stream, event_tx, target).await
}

async fn attach_session_event_stream(
    config: &GlobalConfig,
    target: &SessionTarget,
) -> anyhow::Result<SessionEventStream> {
    let config_snapshot = config.read().clone();
    let client = config_snapshot.nats_client(&target.1).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    SessionEventStream::attach(jetstream, client, &target.0).await
}

async fn forward_session_activity(
    stream: &mut SessionEventStream,
    event_tx: &UnboundedSender<TuiEvent>,
    target: &SessionTarget,
) -> bool {
    while let Some(envelope) = stream.next().await {
        let active = stream
            .should_render(&envelope)
            .then(|| event_activity(&envelope.event))
            .flatten();
        let Some(active) = active else {
            continue;
        };
        if !send_activity(event_tx, target, active) {
            return false;
        }
    }
    true
}

fn send_activity(
    event_tx: &UnboundedSender<TuiEvent>,
    target: &SessionTarget,
    active: bool,
) -> bool {
    event_tx
        .send(TuiEvent::SessionActivity {
            session_id: target.0.clone(),
            cluster: target.1.clone(),
            active,
        })
        .is_ok()
}

fn event_activity(event: &AgentEvent) -> Option<bool> {
    match event {
        AgentEvent::Turn(TurnEvent::Started) => Some(true),
        AgentEvent::Turn(TurnEvent::Ended { .. }) => Some(false),
        AgentEvent::Turn(_)
        | AgentEvent::Model(_)
        | AgentEvent::Tool(_)
        | AgentEvent::Status(_) => Some(true),
        AgentEvent::Session(SessionEvent::CompactingStarted) => Some(true),
        AgentEvent::Session(
            SessionEvent::CompactingCompleted | SessionEvent::CompactingFailed(_),
        ) => Some(false),
        // Nested activity keeps the parent turn busy. A nested Ended event is
        // not the parent session's terminal boundary.
        AgentEvent::SubAgent { .. } => Some(true),
        AgentEvent::Notice(_)
        | AgentEvent::User(_)
        | AgentEvent::Session(_)
        | AgentEvent::Plan { .. } => None,
    }
}

fn history_has_pending_turn(history: &[(u64, SessionLogEntry)]) -> bool {
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(history)
        .unwrap_or_else(|_| history.to_vec());
    let Some(latest_user_seq) = effective.iter().rev().find_map(|(seq, entry)| {
        matches!(entry, SessionLogEntry::Message { role, .. } if role.is_user()).then_some(*seq)
    }) else {
        return false;
    };

    let has_terminal_failure = effective.iter().any(|(seq, entry)| {
        *seq > latest_user_seq
            && matches!(
                entry,
                SessionLogEntry::Error { .. } | SessionLogEntry::Cancel { .. }
            )
    });
    let has_completed_turn = history.iter().any(|(_, entry)| {
        matches!(
            entry,
            SessionLogEntry::TurnEnd { through_seq, .. } if *through_seq >= latest_user_seq
        )
    });

    !has_terminal_failure && !has_completed_turn
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::ModelEvent;
    use harnx_core::message::{MessageContent, MessageRole};

    fn message(role: MessageRole, text: &str) -> SessionLogEntry {
        SessionLogEntry::Message {
            id: None,
            role,
            content: MessageContent::Text(text.to_string()),
            timestamp: None,
            fence_token: None,
        }
    }

    #[test]
    fn pending_history_is_busy_until_the_durable_turn_boundary() {
        let user = vec![(1, message(MessageRole::User, "question"))];
        assert!(history_has_pending_turn(&user));

        let intermediate_answer = vec![
            (1, message(MessageRole::User, "question")),
            (2, message(MessageRole::Assistant, "answer")),
        ];
        assert!(
            history_has_pending_turn(&intermediate_answer),
            "an assistant row can still be followed by stop-hook work"
        );

        let mut completed = intermediate_answer;
        completed.push((
            3,
            SessionLogEntry::TurnEnd {
                through_seq: 1,
                fence_token: 7,
                timestamp: None,
            },
        ));
        assert!(!history_has_pending_turn(&completed));
    }

    #[test]
    fn turn_boundary_does_not_hide_a_queued_user() {
        let history = vec![
            (1, message(MessageRole::User, "first")),
            (2, message(MessageRole::User, "queued")),
            (
                3,
                SessionLogEntry::TurnEnd {
                    through_seq: 1,
                    fence_token: 7,
                    timestamp: None,
                },
            ),
        ];

        assert!(history_has_pending_turn(&history));
    }

    #[test]
    fn only_the_parent_turn_end_marks_shared_activity_idle() {
        assert_eq!(
            event_activity(&AgentEvent::Model(ModelEvent::Final {
                output: "done".to_string(),
                usage: Default::default(),
            })),
            Some(true)
        );
        assert_eq!(
            event_activity(&AgentEvent::Turn(TurnEvent::Ended {
                outcome: Default::default(),
            })),
            Some(false)
        );
        assert_eq!(
            event_activity(&AgentEvent::sub_agent(
                Default::default(),
                AgentEvent::Turn(TurnEvent::Ended {
                    outcome: Default::default(),
                }),
            )),
            Some(true)
        );
    }
}
