//! Subscribe-first transport and durable reconciliation for a shared session.
use super::*;
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::nats_event_sink::{LiveEventState, SessionEventStream};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const DURABLE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
type SessionTarget = (String, String);

enum SessionActivityInput {
    Advisory(Box<harnx_runtime::nats_event_sink::AdvisoryEnvelope>),
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
    live: LiveEventState,
}

impl SessionEventForwarder<'_> {
    fn follows_attach(&self, after_seq: u64) -> bool {
        // Recovery reads advance the durable cursor without rendering the
        // transcript. Only the attachment boundary can filter live events;
        // using the read cursor would discard queued output and handoffs.
        after_seq > self.attached_seq
            || (self.attached_during_turn && after_seq == self.attached_seq)
    }

    fn send_durable_activity(&self, active: bool) -> bool {
        self.send_activity_stamped(active, EventStamp::live(&self.live), true)
    }

    fn send_activity_stamped(&self, active: bool, stamp: EventStamp, historical: bool) -> bool {
        self.send(TuiEvent::SessionActivity {
            historical,
            stamp,
            session_id: self.target.0.clone(),
            cluster: self.target.1.clone(),
            active,
        })
    }

    fn send_agent_event(&self, event: AgentEvent, stamp: EventStamp) -> bool {
        let activity = event_activity(&event);
        self.send_session_event(event, stamp.clone(), false)
            && activity.is_none_or(|active| self.send_activity_stamped(active, stamp, false))
    }

    fn send_session_event(&self, event: AgentEvent, stamp: EventStamp, historical: bool) -> bool {
        self.send(TuiEvent::SessionAgent {
            stamp,
            historical,
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
                self.send_session_event(
                    AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)),
                    EventStamp::snapshot(&self.live),
                    true,
                )
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

pub(super) fn spawn_session_activity_monitor(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    target: SessionTarget,
    live: LiveEventState,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        monitor_session_activity(config, event_tx, target, live).await;
    })
}

async fn monitor_session_activity(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    target: SessionTarget,
    live: LiveEventState,
) {
    loop {
        if !monitor_session_connection(&config, &event_tx, &target, &live).await {
            return;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// A `Cancel` durable history already recorded fences live output by its
/// seq, so a client attaching (or refreshing) after the interrupt landed
/// never renders an advisory that predates it — even one it never issued
/// or otherwise observed itself.
fn fence_live_events_from_history(live: &LiveEventState, history: &[(u64, SessionLogEntry)]) {
    if harnx_core::session_reconstruct::last_terminator_is_cancel(history) {
        live.accept_interrupt(harnx_core::session_reconstruct::last_terminator_seq(
            history,
        ));
    }
}

async fn monitor_session_connection(
    config: &GlobalConfig,
    event_tx: &UnboundedSender<TuiEvent>,
    target: &SessionTarget,
    live: &LiveEventState,
) -> bool {
    let (mut stream, client) = match attach_session_event_stream_with_client(
        config,
        target,
        live.clone(),
    )
    .await
    {
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
    fence_live_events_from_history(live, stream.history());
    // Advisories no longer name a generation (the worker stopped stamping one
    // when interruption moved to the session log); the sequence fence in
    // `should_render` is what now gates individual advisories.
    let attached_during_turn = history_has_pending_turn(stream.history());
    let attached_seq = stream.last_applied_seq();
    let forwarder = SessionEventForwarder {
        event_tx,
        target,
        attached_seq,
        attached_during_turn,
        live: live.clone(),
    };
    // A reconnect can seed its cursor beyond the missed child result while
    // the parent is still busy, so reconcile the attachment history as well.
    if !forwarder.recover_subagent_progress(stream.history(), 0)
        || !forwarder.send_durable_activity(attached_during_turn)
    {
        return false;
    }
    forward_session_activity(&mut stream, &forwarder, client).await
}

pub(crate) async fn attach_session_event_stream_with_state(
    config: &GlobalConfig,
    target: &SessionTarget,
    live: LiveEventState,
) -> anyhow::Result<SessionEventStream> {
    Ok(
        attach_session_event_stream_with_client(config, target, live)
            .await?
            .0,
    )
}

async fn attach_session_event_stream_with_client(
    config: &GlobalConfig,
    target: &SessionTarget,
    live: LiveEventState,
) -> anyhow::Result<(SessionEventStream, async_nats::Client)> {
    let config_snapshot = config.read().clone();
    let client = config_snapshot.nats_client(&target.1).await?;
    let jetstream = async_nats::jetstream::new(client.clone());
    let stream =
        SessionEventStream::attach_with_state(jetstream, client.clone(), &target.0, live).await?;
    Ok((stream, client))
}

async fn forward_session_activity(
    stream: &mut SessionEventStream,
    forwarder: &SessionEventForwarder<'_>,
    client: async_nats::Client,
) -> bool {
    use futures_util::StreamExt;
    let subject =
        harnx_runtime::nats_session_metadata::read_invalidation_subject(&forwarder.target.0);
    let mut read_sub = match client.subscribe(subject).await {
        Ok(sub) => sub,
        Err(error) => {
            log::warn!("Failed to subscribe to read invalidation subject: {error:#}");
            return true;
        }
    };
    let mut active = forwarder.attached_during_turn;
    let mut refresh = tokio::time::interval(DURABLE_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        match next_session_activity_input(stream.next(), active, &mut refresh, read_sub.next())
            .await
        {
            SessionActivityInput::Advisory(envelope) => {
                // Deliberately not `stream.should_render`: that folds in the
                // durable read cursor, which a refresh can advance past a
                // still-queued advisory (a `HandoffCommitted` has no other
                // delivery path) and drop it forever. The attachment and
                // cancel fence are the only things allowed to gate a live
                // advisory here; `follows_attach` below is the sole cursor
                // check, and it reads the attachment boundary, never the
                // read cursor.
                let should_render = forwarder.live.should_render(&envelope, 0);
                if !forward_advisory(forwarder, *envelope, should_render, &mut active) {
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
        message = read_invalidation => if message.is_some() { SessionActivityInput::ReadInvalidation } else { SessionActivityInput::SubscriptionClosed },
        envelope = advisory => envelope.map_or(
            SessionActivityInput::SubscriptionClosed,
            |envelope| SessionActivityInput::Advisory(Box::new(envelope)),
        ),
    }
}

fn forward_advisory(
    forwarder: &SessionEventForwarder<'_>,
    envelope: harnx_runtime::nats_event_sink::AdvisoryEnvelope,
    should_render: bool,
    active: &mut bool,
) -> bool {
    if !should_render || !forwarder.follows_attach(envelope.after_seq) {
        return true;
    }
    let activity = event_activity(&envelope.event);
    if let Some(next) = activity {
        *active = next;
    }
    forwarder.send_agent_event(envelope.event, EventStamp::live(&forwarder.live))
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
    fence_live_events_from_history(&forwarder.live, stream.history());
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
    if forwarder.send_durable_activity(durable_activity) {
        DurableRefreshOutcome::Continue
    } else {
        DurableRefreshOutcome::Stop
    }
}

fn event_activity(event: &AgentEvent) -> Option<bool> {
    match event {
        AgentEvent::Turn(TurnEvent::Started) => Some(true),
        AgentEvent::Turn(TurnEvent::Ended { .. } | TurnEvent::Interrupted { .. }) => Some(false),
        AgentEvent::Turn(_)
        | AgentEvent::Model(_)
        | AgentEvent::Tool(_)
        | AgentEvent::Status(_) => Some(true),
        AgentEvent::Session(SessionEvent::CompactingStarted { .. }) => Some(true),
        AgentEvent::Session(
            SessionEvent::CompactingCompleted { .. } | SessionEvent::CompactingFailed { .. },
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

pub(crate) fn history_has_pending_turn(history: &[(u64, SessionLogEntry)]) -> bool {
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
#[path = "recovery_tests.rs"]
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
            next_session_activity_input(advisory, true, &mut refresh, std::future::pending()).await,
            SessionActivityInput::RefreshDurableHistory
        ));
    }

    #[tokio::test]
    async fn closed_read_subscription_requests_reconnection() {
        let mut refresh = tokio::time::interval(Duration::from_secs(1));
        let input = next_session_activity_input(
            std::future::pending(),
            false,
            &mut refresh,
            std::future::ready(None),
        )
        .await;
        assert!(matches!(input, SessionActivityInput::SubscriptionClosed));
    }

    #[test]
    fn read_invalidation_remains_visible_after_interrupt_is_accepted() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let target = ("session".to_string(), "cluster".to_string());
        let live = LiveEventState::default();
        live.accept_interrupt(1);
        let forwarder = SessionEventForwarder {
            event_tx: &event_tx,
            target: &target,
            attached_seq: 10,
            attached_during_turn: false,
            live,
        };
        assert!(forwarder.send_session_read_invalidation());
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::SessionReadInvalidation { session_id }) if session_id == target.0
        ));
        assert!(event_rx.try_recv().is_err());
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
            event_activity(&AgentEvent::Turn(TurnEvent::Interrupted {
                cancellation_id: "c".into()
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
            live: LiveEventState::default(),
        };

        assert!(forwarder.send_agent_event(
            AgentEvent::Turn(TurnEvent::Ended {
                outcome: Default::default(),
            }),
            EventStamp::snapshot(&forwarder.live)
        ));
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
            live: LiveEventState::default(),
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
