//! Independent subscribe-first monitors for nested agent session transcripts.

use crate::session_activity::{attach_session_event_stream, history_has_pending_turn};
use crate::types::{MonitoredSessionKey, SubAgentStatus, TranscriptItem, Tui, TuiEvent};
use harnx_core::event::{AgentEvent, ModelEvent, TurnEvent};
use harnx_core::session::SessionLogEntry;
use harnx_runtime::config::GlobalConfig;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttachmentOutcome {
    Terminal,
    Disconnected,
    AttachFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AttachmentBoundary {
    attached_seq: u64,
    attached_during_turn: bool,
}

impl AttachmentBoundary {
    fn follows_attach(self, after_seq: u64) -> bool {
        self.attached_during_turn || after_seq > self.attached_seq
    }
}

impl Tui {
    pub(super) fn sync_subagent_monitor_root(&mut self) {
        let desired = self.session_activity_destination();
        if desired == self.subagent_monitor_root {
            return;
        }
        self.stop_subagent_monitors();
        self.app.monitored_sessions.clear();
        self.app.subagent_view_stack.clear();
        if self.app.detail_view_entry.take().is_some() {
            self.app.detail_view_open = false;
        }
        self.subagent_monitor_root = desired;
        self.subagent_rows_dirty = true;
    }

    pub(super) fn sync_known_subagent_rows(&mut self) {
        if !std::mem::take(&mut self.subagent_rows_dirty) {
            return;
        }
        let mut keys = subagent_rows(&self.app.transcript);
        for state in self.app.monitored_sessions.values() {
            keys.extend(subagent_rows(&state.transcript));
        }
        keys.sort_by(|left, right| {
            (&left.cluster, &left.agent, &left.session_id).cmp(&(
                &right.cluster,
                &right.agent,
                &right.session_id,
            ))
        });
        keys.dedup();
        for key in keys {
            self.ensure_subagent_monitor(key);
        }
    }

    pub(super) fn ensure_subagent_monitor(&mut self, key: MonitoredSessionKey) {
        self.app
            .monitored_sessions
            .entry(key.clone())
            .or_insert_with(|| crate::types::MonitoredSessionState::new(SubAgentStatus::Running));
        let monitor_running = self
            .subagent_monitor_handles
            .get(&key)
            .is_some_and(|handle| !handle.is_finished());
        if monitor_running {
            return;
        }
        if let Some(handle) = self.subagent_monitor_handles.remove(&key) {
            handle.abort();
        }
        let handle =
            spawn_subagent_monitor(self.config.clone(), self.event_tx.clone(), key.clone());
        self.subagent_monitor_handles.insert(key, handle);
    }

    pub(super) fn stop_subagent_monitors(&mut self) {
        for (_, handle) in self.subagent_monitor_handles.drain() {
            handle.abort();
        }
    }
}

fn subagent_rows(transcript: &[TranscriptItem]) -> Vec<MonitoredSessionKey> {
    transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::SubAgentSession { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect()
}

fn spawn_subagent_monitor(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    key: MonitoredSessionKey,
) -> JoinHandle<()> {
    tokio::spawn(monitor_subagent_session(config, event_tx, key))
}

async fn monitor_subagent_session(
    config: GlobalConfig,
    event_tx: UnboundedSender<TuiEvent>,
    key: MonitoredSessionKey,
) {
    let target = (key.session_id.clone(), key.cluster.clone());
    let mut reconnect_delay = RECONNECT_DELAY;
    loop {
        let outcome = monitor_subagent_attachment(&config, &event_tx, &key, &target).await;
        if outcome == AttachmentOutcome::Terminal {
            return;
        }
        if event_tx.is_closed() {
            return;
        }
        if outcome == AttachmentOutcome::AttachFailed {
            tokio::time::sleep(reconnect_delay).await;
            reconnect_delay = reconnect_delay
                .checked_mul(2)
                .unwrap_or(MAX_RECONNECT_DELAY)
                .min(MAX_RECONNECT_DELAY);
        } else {
            reconnect_delay = RECONNECT_DELAY;
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }
}

async fn monitor_subagent_attachment(
    config: &GlobalConfig,
    event_tx: &UnboundedSender<TuiEvent>,
    key: &MonitoredSessionKey,
    target: &(String, String),
) -> AttachmentOutcome {
    let mut stream = match attach_session_event_stream(config, target).await {
        Ok(stream) => stream,
        Err(error) => {
            log::debug!(
                "failed to attach sub-agent monitor: agent={} session_id={} cluster={} error={error:#}",
                key.agent,
                key.session_id,
                key.cluster,
            );
            return AttachmentOutcome::AttachFailed;
        }
    };
    let boundary = AttachmentBoundary {
        attached_seq: stream.last_applied_seq(),
        attached_during_turn: history_has_pending_turn(stream.history()),
    };
    let Some(status) = send_subagent_snapshot(config, event_tx, key, stream.history()).await else {
        return AttachmentOutcome::Disconnected;
    };
    if status != SubAgentStatus::Running {
        return AttachmentOutcome::Terminal;
    }
    while let Some(envelope) = stream.next().await {
        if !stream.should_render(&envelope) || !boundary.follows_attach(envelope.after_seq) {
            continue;
        }
        let terminal = is_subagent_terminal_event(&envelope.event);
        if event_tx
            .send(TuiEvent::SubAgentSessionEvent {
                key: key.clone(),
                event: envelope.event,
            })
            .is_err()
        {
            return AttachmentOutcome::Disconnected;
        }
        if terminal {
            return match refresh_terminal_subagent_snapshot(config, event_tx, key).await {
                Some(SubAgentStatus::Completed | SubAgentStatus::Failed) => {
                    AttachmentOutcome::Terminal
                }
                Some(SubAgentStatus::Running) | None => AttachmentOutcome::Disconnected,
            };
        }
    }
    AttachmentOutcome::Disconnected
}

async fn refresh_terminal_subagent_snapshot(
    config: &GlobalConfig,
    event_tx: &UnboundedSender<TuiEvent>,
    key: &MonitoredSessionKey,
) -> Option<SubAgentStatus> {
    let client = stream_client(config, &key.cluster).await?;
    let log = harnx_runtime::nats_session_log::NatsSessionLog::new(
        async_nats::jetstream::new(client),
        key.session_id.clone(),
    );
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let Ok(history) = log.load_events_async().await else {
            continue;
        };
        if subagent_history_status(&history) == SubAgentStatus::Running {
            continue;
        }
        return send_subagent_snapshot(config, event_tx, key, &history).await;
    }
    Some(SubAgentStatus::Running)
}

async fn stream_client(config: &GlobalConfig, cluster: &str) -> Option<async_nats::Client> {
    let snapshot = config.read().clone();
    snapshot.nats_client(cluster).await.ok()
}

async fn send_subagent_snapshot(
    config: &GlobalConfig,
    event_tx: &UnboundedSender<TuiEvent>,
    key: &MonitoredSessionKey,
    history: &[(u64, SessionLogEntry)],
) -> Option<SubAgentStatus> {
    let status = subagent_history_status(history);
    event_tx
        .send(TuiEvent::SubAgentSessionSnapshot {
            key: key.clone(),
            transcript: load_subagent_transcript(config, key).await,
            status: status.clone(),
        })
        .is_ok()
        .then_some(status)
}

async fn load_subagent_transcript(
    config: &GlobalConfig,
    key: &MonitoredSessionKey,
) -> Vec<TranscriptItem> {
    let state = match crate::session_history_loader::load_remote_session_history(
        config,
        key.agent.clone(),
        key.cluster.clone(),
        key.session_id.clone(),
    )
    .await
    {
        Ok(state) => state,
        Err(error) => {
            return vec![TranscriptItem::ErrorText(format!(
                "Failed to load sub-agent session history: {error:#}"
            ))];
        }
    };
    let mut transcript = crate::lifecycle::build_transcript_with_compaction_for_cluster(
        &state.compressed_messages,
        &state.messages,
        state.compaction_summary.as_deref(),
        &std::collections::HashMap::new(),
        Some(&key.cluster),
    );
    transcript.extend(
        state.replay_warnings.into_iter().map(|warning| {
            TranscriptItem::ErrorText(format!("Session history warning: {warning}"))
        }),
    );
    transcript
}

fn is_subagent_terminal_event(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Turn(TurnEvent::Ended { .. }) | AgentEvent::Model(ModelEvent::Error(_))
    )
}

fn subagent_history_status(history: &[(u64, SessionLogEntry)]) -> SubAgentStatus {
    if history.is_empty() || history_has_pending_turn(history) {
        return SubAgentStatus::Running;
    }
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(history)
        .unwrap_or_else(|_| history.to_vec());
    if latest_user_failed(&effective) {
        SubAgentStatus::Failed
    } else {
        SubAgentStatus::Completed
    }
}

fn latest_user_failed(history: &[(u64, SessionLogEntry)]) -> bool {
    let latest_user_seq = history.iter().rev().find_map(|(seq, entry)| {
        matches!(entry, SessionLogEntry::Message { role, .. } if role.is_user()).then_some(*seq)
    });
    latest_user_seq.is_some_and(|user_seq| {
        history.iter().any(|(seq, entry)| {
            *seq > user_seq
                && matches!(
                    entry,
                    SessionLogEntry::Error { .. } | SessionLogEntry::Cancel { .. }
                )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_child_history_stays_running_until_the_prompt_is_visible() {
        assert_eq!(subagent_history_status(&[]), SubAgentStatus::Running);
    }

    #[test]
    fn reconnect_backoff_is_bounded() {
        let mut delay = RECONNECT_DELAY;
        for _ in 0..10 {
            delay = delay
                .checked_mul(2)
                .unwrap_or(MAX_RECONNECT_DELAY)
                .min(MAX_RECONNECT_DELAY);
        }
        assert_eq!(delay, MAX_RECONNECT_DELAY);
    }

    #[test]
    fn child_attach_boundary_skips_advisories_already_covered_by_history() {
        let completed = AttachmentBoundary {
            attached_seq: 7,
            attached_during_turn: false,
        };
        assert!(!completed.follows_attach(7));
        assert!(completed.follows_attach(8));

        let active = AttachmentBoundary {
            attached_during_turn: true,
            ..completed
        };
        assert!(active.follows_attach(7));
    }
}
