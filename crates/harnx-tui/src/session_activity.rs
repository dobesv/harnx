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
    ReadInvalidation,
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
    fn follows_attach(&self, after_seq: u64) -> bool {
        // Recovery reads advance the durable cursor without rendering the
        // transcript. Only the attachment boundary can filter live events;
        // using the read cursor would discard queued output and handoffs.
        after_seq > self.attached_seq
            || (self.attached_during_turn && after_seq == self.attached_seq)
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
        self.send_session_event(event) && activity.is_none_or(|active| self.send_activity(active))
    }

    fn send_session_event(&self, event: AgentEvent) -> bool {
        self.send(TuiEvent::SessionAgent {
            session_id: self.target.0.clone(),
            cluster: self.target.1.clone(),
            event,
        })
    }

    fn recover_subagent_progress(
        &self,
        history: &[(u64, SessionLogEntry)],
        after_seq: u64,
    ) -> bool {
        let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(history)
            .unwrap_or_else(|_| history.to_vec());
        // Compaction trims model context, but archived rows and open child
        // views remain visible and still need their terminal progress.
        harnx_runtime::nats_session::completed_subagent_progress(&effective, after_seq)
            .into_iter()
            .all(|progress| {
                self.send_session_event(AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)))
            })
    }
    fn send_session_read_invalidation(&self) -> bool {
        self.send(TuiEvent::SessionReadInvalidation {
            session_id: self.target.0.clone(),
        })
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
            self.active_remote_session = Some(target.clone());
            self.refresh_input_chrome();
            // Reopening a session can reveal a durable pending turn after its
            // prior worker vanished. Re-activation is idempotent under the
            // session lease and guarantees that "busy" has an owner that can
            // either resume successfully or write a terminal Error.
            #[cfg(not(test))]
            if let Err(error) = self
                .activate_pending_session(target.0.clone(), target.1.clone())
                .await
            {
                log::warn!(
                    "failed to reactivate pending attached session {}: {error:#}",
                    target.0
                );
            }
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
        let confirmation_target = self.session_activity_destination();
        self.sync_tool_confirmation_route(confirmation_target.as_ref());
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

    /// Handle a read invalidation event for a session.
    /// Updates the cached unread state if it matches the current session.
    /// Also refreshes any open SessionPicker modal to update the unread markers.
    pub(super) async fn handle_session_read_invalidation(&mut self, session_id: &str) {
        let current = self.session_activity_destination();
        let is_current_session = current.as_ref().map(|(id, _)| id.as_str()) == Some(session_id);

        // If this is the current session, update the cached unread state
        if is_current_session {
            if let Some((_, cluster)) = current.clone() {
                let config = self.config.read().clone();
                if let Ok(jetstream) = config.nats_jetstream(&cluster).await {
                    if let Ok(store) =
                        harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(
                            &jetstream, 1,
                        )
                        .await
                    {
                        if let Ok(read_state) = store.get_read_state(session_id).await {
                            self.app.current_session_unread = read_state.is_unread();
                            self.refresh_input_chrome();
                        }
                    }
                }
            }
        }

        // If a SessionPicker modal is open, refresh its session list to update unread markers
        if let Some(crate::types::ModalState::SessionPicker {
            sessions: _,
            selected,
            origin_agent,
            origin_session,
            error: _,
        }) = &self.app.modal
        {
            let (sessions, fetch_error) = Self::picker_sessions(&self.config).await;
            self.app.modal = Some(crate::types::ModalState::SessionPicker {
                sessions,
                selected: *selected,
                origin_agent: origin_agent.clone(),
                origin_session: origin_session.clone(),
                error: fetch_error,
            });
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.clear_tool_confirmation_route();
        self.stop_session_activity_monitor();
        self.stop_subagent_monitors();
    }
}

fn spawn_session_activity_monitor(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    target: SessionTarget,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::join!(
            crate::cancellation::monitor_execution(&config, &event_tx, &target),
            monitor_session_activity(config.clone(), event_tx.clone(), target.clone())
        );
    })
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
    let (mut stream, client) = match attach_session_event_stream(config, target).await {
        Ok(result) => result,
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
    // A reconnect can seed its cursor beyond the missed child result while
    // the parent is still busy, so reconcile the attachment history as well.
    if !forwarder.recover_subagent_progress(stream.history(), 0)
        || !forwarder.send_activity(attached_during_turn)
    {
        return false;
    }
    forward_session_activity(&mut stream, &forwarder, client).await
}

pub(super) async fn attach_session_event_stream(
    config: &GlobalConfig,
    target: &SessionTarget,
) -> anyhow::Result<(SessionEventStream, async_nats::Client)> {
    let config_snapshot = config.read().clone();
    let client = config_snapshot.nats_client(&target.1).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let stream = SessionEventStream::attach(jetstream, client.clone(), &target.0).await?;
    Ok((stream, client))
}

async fn forward_session_activity(
    stream: &mut SessionEventStream,
    forwarder: &SessionEventForwarder<'_>,
    client: async_nats::Client,
) -> bool {
    let mut active = forwarder.attached_during_turn;
    let mut refresh = tokio::time::interval(DURABLE_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Subscribe to read invalidation notifications for this session
    // Create a persistent subscriber that lives for the forwarder's lifetime
    use futures_util::StreamExt;
    use harnx_runtime::nats_session_metadata::read_invalidation_subject;

    let subject = read_invalidation_subject(&forwarder.target.0);
    let mut read_sub = match client.subscribe(subject.clone()).await {
        Ok(sub) => sub,
        Err(e) => {
            log::warn!("Failed to subscribe to read invalidation subject: {e:#}");
            return false;
        }
    };

    loop {
        match next_session_activity_input(stream.next(), active, &mut refresh, read_sub.next())
            .await
        {
            SessionActivityInput::Advisory(envelope) => {
                if !forward_advisory(forwarder, envelope, &mut active) {
                    return false;
                }
            }
            SessionActivityInput::RefreshDurableHistory => {
                let outcome = refresh_durable_activity(stream, forwarder, &mut active).await;
                // Leave time for advisories even when a durable read takes
                // longer than the refresh interval.
                refresh.reset();
                match outcome {
                    DurableRefreshOutcome::Continue => {}
                    DurableRefreshOutcome::Reconnect => return true,
                    DurableRefreshOutcome::Stop => return false,
                }
            }
            SessionActivityInput::SubscriptionClosed => return true,
            SessionActivityInput::ReadInvalidation => {
                if !forwarder.send_session_read_invalidation() {
                    return false;
                }
                // Subscriber is persistent; no need to re-subscribe
            }
        }
    }
}

async fn next_session_activity_input(
    advisory: impl std::future::Future<
        Output = Option<harnx_runtime::nats_event_sink::AdvisoryEnvelope>,
    >,
    active: bool,
    refresh: &mut tokio::time::Interval,
    read_invalidation: impl std::future::Future<Output = Option<async_nats::Message>>,
) -> SessionActivityInput {
    // A fresh timeout per advisory starves durable recovery while the parent
    // keeps streaming. The interval survives each advisory and wins when due.
    tokio::select! {
        biased;
        _ = refresh.tick(), if active => SessionActivityInput::RefreshDurableHistory,
        _ = read_invalidation => SessionActivityInput::ReadInvalidation,
        envelope = advisory => envelope.map_or(
            SessionActivityInput::SubscriptionClosed,
            SessionActivityInput::Advisory,
        ),
    }
}

fn forward_advisory(
    forwarder: &SessionEventForwarder<'_>,
    envelope: harnx_runtime::nats_event_sink::AdvisoryEnvelope,
    active: &mut bool,
) -> bool {
    if !forwarder.follows_attach(envelope.after_seq) {
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
    let after_seq = stream.last_applied_seq();
    if let Err(error) = stream.refresh_history().await {
        log::debug!(
            "failed to refresh durable session activity: session_id={} cluster={} error={error:#}",
            forwarder.target.0,
            forwarder.target.1,
        );
        return DurableRefreshOutcome::Reconnect;
    }
    // Completion of a child does not end the parent's turn. Repair its row
    // before checking whether root activity changed, without replaying output.
    if !forwarder.recover_subagent_progress(stream.history(), after_seq) {
        return DurableRefreshOutcome::Stop;
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
#[path = "session_activity/recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::ModelEvent;
    use harnx_core::message::{MessageContent, MessageRole};

    #[tokio::test]
    async fn durable_refresh_is_not_starved_by_ready_advisories() {
        let mut refresh = tokio::time::interval(Duration::from_millis(1));
        refresh.tick().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        let advisory =
            std::future::ready(Some(harnx_runtime::nats_event_sink::AdvisoryEnvelope::new(
                1,
                AgentEvent::Turn(TurnEvent::Started),
            )));
        assert!(matches!(
            next_session_activity_input(
                advisory,
                true,
                &mut refresh,
                std::future::pending::<Option<async_nats::Message>>()
            )
            .await,
            SessionActivityInput::RefreshDurableHistory
        ));
    }

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
                usage: None,
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
                    usage: None,
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
        assert!(!completed.follows_attach(6));
        assert!(!completed.follows_attach(7));
        assert!(completed.follows_attach(8));

        let active = SessionEventForwarder {
            attached_during_turn: true,
            ..completed
        };
        assert!(!active.follows_attach(6));
        assert!(active.follows_attach(7));
    }
}
