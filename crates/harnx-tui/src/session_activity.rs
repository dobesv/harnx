use crate::types::{Tui, TuiEvent};
use harnx_core::event::{AgentEvent, SessionEvent, TurnEvent};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::{GlobalConfig, LOCAL_CLUSTER_KEY};
use harnx_runtime::nats_event_sink::SessionEventStream;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const DURABLE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
type SessionTarget = (String, String);

enum SessionActivityInput {
    Advisory(harnx_runtime::nats_event_sink::AdvisoryEnvelope),
    RefreshDurableHistory,
    SubscriptionClosed,
}

enum DurableRefreshOutcome {
    Continue,
    Reconnect,
    Stop,
}

struct SessionEventForwarder<'a> {
    event_tx: &'a UnboundedSender<TuiEvent>,
    target: &'a SessionTarget,
    attached_seq: u64,
    attached_during_turn: bool,
}

impl SessionEventForwarder<'_> {
    fn should_forward(
        &self,
        stream: &SessionEventStream,
        envelope: &harnx_runtime::nats_event_sink::AdvisoryEnvelope,
    ) -> bool {
        stream.should_render(envelope) && self.follows_attach(envelope.after_seq)
    }

    fn follows_attach(&self, after_seq: u64) -> bool {
        self.attached_during_turn || after_seq > self.attached_seq
    }

    fn send_activity(&self, active: bool) -> bool {
        self.send(TuiEvent::SessionActivity {
            session_id: self.target.0.clone(),
            cluster: self.target.1.clone(),
            active,
        })
    }

    fn send_agent_event(&self, event: AgentEvent) -> bool {
        let activity = event_activity(&event);
        self.send(TuiEvent::SessionAgent {
            session_id: self.target.0.clone(),
            cluster: self.target.1.clone(),
            event,
        }) && activity.is_none_or(|active| self.send_activity(active))
    }

    fn send(&self, event: TuiEvent) -> bool {
        self.event_tx.send(event).is_ok()
    }
}

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
            // A reconnect may miss the lossy TurnEnded advisory. If this TUI
            // previously observed the foreign turn, converge from its durable
            // history before clearing the busy state.
            if self.active_remote_session.as_ref() == Some(&target) {
                self.refresh_shared_session_transcript().await;
            }
            self.complete_main_prompt().await;
        }
    }

    pub(super) async fn handle_shared_session_agent_event(
        &mut self,
        session_id: String,
        cluster: String,
        event: AgentEvent,
    ) {
        let target = (session_id, cluster);
        if self.session_activity_target.as_ref() != Some(&target) {
            return;
        }
        // The locally-owned prompt already delivers the same worker events
        // through TuiAgentEventSink. Rendering the shared fan-out as well would
        // duplicate every streamed chunk and tool row.
        if self.current_prompt_abort.is_some() {
            return;
        }

        let refresh_before = matches!(event, AgentEvent::Turn(TurnEvent::Started));
        let refresh_after = matches!(event, AgentEvent::Turn(TurnEvent::Ended { .. }));
        if refresh_before {
            self.refresh_shared_session_transcript().await;
        }
        self.render_agent_event(event).await;
        if refresh_after {
            // Advisory fan-out is deliberately lossy. Rebuild at the durable
            // turn boundary so a missed final chunk still converges exactly to
            // what reopening the session would show.
            self.refresh_shared_session_transcript().await;
        }
    }

    async fn refresh_shared_session_transcript(&mut self) {
        self.app.transcript =
            crate::lifecycle::session_history_transcript_items(&self.config).await;
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.last_ui_output_source = None;
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.subagent_rows_dirty = true;
        self.pin_transcript_to_bottom();
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
        self.sync_subagent_monitor_root();
        self.sync_known_subagent_rows();
        let desired = self
            .current_prompt_abort
            .is_none()
            .then(|| self.session_activity_destination())
            .flatten();
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

    pub(super) fn session_activity_destination(&self) -> Option<(String, String)> {
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
        self.stop_subagent_monitors();
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
    let attached_during_turn = history_has_pending_turn(stream.history());
    let attached_seq = stream.last_applied_seq();
    let forwarder = SessionEventForwarder {
        event_tx,
        target,
        attached_seq,
        attached_during_turn,
    };
    if !forwarder.send_activity(attached_during_turn) {
        return false;
    }
    forward_session_activity(&mut stream, &forwarder).await
}

pub(super) async fn attach_session_event_stream(
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
    forwarder: &SessionEventForwarder<'_>,
) -> bool {
    let mut active = forwarder.attached_during_turn;
    loop {
        match next_session_activity_input(stream, active).await {
            SessionActivityInput::Advisory(envelope) => {
                if !forward_advisory(stream, forwarder, envelope, &mut active) {
                    return false;
                }
            }
            SessionActivityInput::RefreshDurableHistory => {
                match refresh_durable_activity(stream, forwarder, &mut active).await {
                    DurableRefreshOutcome::Continue => {}
                    DurableRefreshOutcome::Reconnect => return true,
                    DurableRefreshOutcome::Stop => return false,
                }
            }
            SessionActivityInput::SubscriptionClosed => return true,
        }
    }
}

async fn next_session_activity_input(
    stream: &mut SessionEventStream,
    active: bool,
) -> SessionActivityInput {
    if !active {
        return stream.next().await.map_or(
            SessionActivityInput::SubscriptionClosed,
            SessionActivityInput::Advisory,
        );
    }
    match tokio::time::timeout(DURABLE_REFRESH_INTERVAL, stream.next()).await {
        Ok(Some(envelope)) => SessionActivityInput::Advisory(envelope),
        Ok(None) => SessionActivityInput::SubscriptionClosed,
        Err(_) => SessionActivityInput::RefreshDurableHistory,
    }
}

fn forward_advisory(
    stream: &SessionEventStream,
    forwarder: &SessionEventForwarder<'_>,
    envelope: harnx_runtime::nats_event_sink::AdvisoryEnvelope,
    active: &mut bool,
) -> bool {
    if !forwarder.should_forward(stream, &envelope) {
        return true;
    }
    let activity = event_activity(&envelope.event);
    if let Some(next) = activity {
        *active = next;
    }
    forwarder.send_agent_event(envelope.event)
}

async fn refresh_durable_activity(
    stream: &mut SessionEventStream,
    forwarder: &SessionEventForwarder<'_>,
    active: &mut bool,
) -> DurableRefreshOutcome {
    if let Err(error) = stream.refresh_history().await {
        log::debug!(
            "failed to refresh durable session activity: session_id={} cluster={} error={error:#}",
            forwarder.target.0,
            forwarder.target.1,
        );
        return DurableRefreshOutcome::Reconnect;
    }
    let durable_activity = history_has_pending_turn(stream.history());
    if durable_activity == *active {
        return DurableRefreshOutcome::Continue;
    }
    *active = durable_activity;
    if forwarder.send_activity(durable_activity) {
        DurableRefreshOutcome::Continue
    } else {
        DurableRefreshOutcome::Stop
    }
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

pub(super) fn history_has_pending_turn(history: &[(u64, SessionLogEntry)]) -> bool {
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

    #[test]
    fn agent_event_precedes_its_activity_transition() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let target = ("session".to_string(), "cluster".to_string());
        let forwarder = SessionEventForwarder {
            event_tx: &event_tx,
            target: &target,
            attached_seq: 0,
            attached_during_turn: true,
        };

        assert!(
            forwarder.send_agent_event(AgentEvent::Turn(TurnEvent::Ended {
                outcome: Default::default(),
            }))
        );
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SessionAgent { .. })
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SessionActivity { active: false, .. })
        ));
    }

    #[test]
    fn completed_tail_advisories_are_not_replayed_after_attach() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let target = ("session".to_string(), "cluster".to_string());
        let completed = SessionEventForwarder {
            event_tx: &event_tx,
            target: &target,
            attached_seq: 7,
            attached_during_turn: false,
        };
        assert!(!completed.follows_attach(7));
        assert!(completed.follows_attach(8));

        let active = SessionEventForwarder {
            attached_during_turn: true,
            ..completed
        };
        assert!(active.follows_attach(7));
    }
}
