//! Per-invocation sub-agent progress state and transcript correlation.

use crate::subagent_transcript::update_row;
use crate::types::{
    MonitoredSessionKey, MonitoredSessionState, SubAgentInvocationProgress, SubAgentStatus,
    TranscriptItem, Tui,
};
use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};

struct RowUpdate {
    key: MonitoredSessionKey,
    status: SubAgentStatus,
    invocation_id: Option<String>,
    progress: Option<SubAgentInvocationProgress>,
}

/// What an update did to a sub-agent's transcript row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowChange {
    /// The row already holds a terminal snapshot, which nothing reopens.
    Rejected,
    Updated,
    Created,
}

impl Tui {
    pub(super) fn record_subagent_completed(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        key: MonitoredSessionKey,
    ) {
        self.upsert_subagent_row(
            parent,
            RowUpdate {
                key: key.clone(),
                status: SubAgentStatus::Completed,
                invocation_id: None,
                progress: None,
            },
        );
        let state = self
            .app
            .monitored_sessions
            .entry(key.clone())
            .or_insert_with(|| MonitoredSessionState::new(SubAgentStatus::Completed));
        if state.status != SubAgentStatus::Failed {
            state.status = SubAgentStatus::Completed;
        }
        let status = state.status.clone();
        self.update_subagent_row_status(&key, status);
        self.ensure_subagent_monitor(key);
        self.pin_transcript_to_bottom();
    }

    pub(super) fn record_subagent_progress(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        snapshot: SubAgentProgress,
    ) {
        if !valid_snapshot(&snapshot) {
            return;
        }
        let key = MonitoredSessionKey {
            cluster: parent.map_or_else(
                || self.current_session_cluster(),
                |parent| parent.cluster.clone(),
            ),
            agent: snapshot.agent.clone(),
            session_id: snapshot.session_id.clone(),
        };
        let invocation_id = snapshot.invocation_id.clone();
        let running = snapshot.status == SubAgentProgressStatus::Running;
        let status = SubAgentStatus::from_progress(snapshot.status);
        let progress = SubAgentInvocationProgress::new(snapshot);
        let change = self.upsert_subagent_row(
            parent,
            RowUpdate {
                key: key.clone(),
                status: status.clone(),
                invocation_id: Some(invocation_id.clone()),
                progress: Some(progress.clone()),
            },
        );
        if change == RowChange::Rejected {
            return;
        }
        self.app
            .monitored_sessions
            .entry(key.clone())
            .or_insert_with(|| MonitoredSessionState::new(status.clone()))
            .status = status.clone();
        // Only a running snapshot that opens a row moves the monitor, because
        // that is the first sight of an invocation, normally its start. A
        // terminal snapshot replayed from history must not pull a live monitor
        // backwards, and the heartbeats of two invocations running on one
        // child at once must not swap it back and forth, reloading the child's
        // log every time.
        if running && change == RowChange::Created {
            self.follow_invocation(&key, &invocation_id);
        }
        self.show_in_open_views(&invocation_id, &status, &progress);
        self.ensure_subagent_monitor(key);
        self.pin_transcript_to_bottom();
    }

    /// Show an invocation's latest status and metrics in every open view of it.
    fn show_in_open_views(
        &mut self,
        invocation_id: &str,
        status: &SubAgentStatus,
        progress: &SubAgentInvocationProgress,
    ) {
        for view in &mut self.app.subagent_view_stack {
            if view_matches_invocation(view.progress.as_ref(), invocation_id) {
                view.status = status.clone();
                view.progress = Some(progress.clone());
            }
        }
    }

    fn upsert_subagent_row(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        update: RowUpdate,
    ) -> RowChange {
        let transcript = match parent {
            Some(parent) => {
                &mut self
                    .app
                    .monitored_sessions
                    .entry(parent.clone())
                    .or_insert_with(|| MonitoredSessionState::new(SubAgentStatus::Running))
                    .transcript
            }
            None => &mut self.app.transcript,
        };
        match update.invocation_id.clone() {
            Some(invocation_id) => upsert_invocation(transcript, update, invocation_id),
            None => upsert_legacy(transcript, update),
        }
    }

    /// Point the child's monitor at `invocation_id`. A child prompted again
    /// runs a new turn, and its monitor has to follow that turn rather than the
    /// one it was started for.
    fn follow_invocation(&mut self, key: &MonitoredSessionKey, invocation_id: &str) {
        let Some(state) = self.app.monitored_sessions.get_mut(key) else {
            return;
        };
        if state.invocation_id.as_deref() == Some(invocation_id) {
            return;
        }
        state.invocation_id = Some(invocation_id.to_string());
        state.streaming_open = false;
        if let Some(handle) = self.subagent_monitor_handles.remove(key) {
            handle.abort();
        }
    }

    pub(super) fn update_subagent_row_status(
        &mut self,
        key: &MonitoredSessionKey,
        status: SubAgentStatus,
    ) {
        update_row(&mut self.app.transcript, key, &status);
        for state in self.app.monitored_sessions.values_mut() {
            update_row(&mut state.transcript, key, &status);
        }
        for view in &mut self.app.subagent_view_stack {
            if &view.key == key && view.progress.is_none() {
                view.status = status.clone();
            }
        }
    }
}

fn valid_snapshot(snapshot: &SubAgentProgress) -> bool {
    if snapshot.invocation_id.trim().is_empty() {
        return false;
    }
    if snapshot.agent.trim().is_empty() {
        return false;
    }
    !snapshot.session_id.trim().is_empty()
}

fn view_matches_invocation(
    progress: Option<&SubAgentInvocationProgress>,
    invocation_id: &str,
) -> bool {
    progress.is_some_and(|current| current.snapshot.invocation_id == invocation_id)
}

fn upsert_invocation(
    transcript: &mut Vec<TranscriptItem>,
    update: RowUpdate,
    invocation_id: String,
) -> RowChange {
    let existing = transcript.iter_mut().find(|item| {
        matches!(
            item,
            TranscriptItem::SubAgentSession {
                invocation_id: Some(row_invocation_id),
                ..
            } if row_invocation_id == &invocation_id
        )
    });
    if let Some(TranscriptItem::SubAgentSession {
        status, progress, ..
    }) = existing
    {
        if progress.as_ref().is_some_and(|current| {
            matches!(
                current.snapshot.status,
                SubAgentProgressStatus::Done
                    | SubAgentProgressStatus::Failed
                    | SubAgentProgressStatus::Cancelled
            )
        }) {
            return RowChange::Rejected;
        }
        *status = update.status;
        if update.progress.is_some() {
            *progress = update.progress;
        }
        return RowChange::Updated;
    }
    transcript.push(TranscriptItem::SubAgentSession {
        key: update.key,
        status: update.status,
        invocation_id: Some(invocation_id),
        progress: update.progress,
    });
    RowChange::Created
}

fn upsert_legacy(transcript: &mut Vec<TranscriptItem>, update: RowUpdate) -> RowChange {
    let latest_tool = transcript
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::ToolCall { .. }));
    let latest_row = transcript.iter().rposition(
        |item| matches!(item, TranscriptItem::SubAgentSession { key, .. } if key == &update.key),
    );
    let row_follows_tool = latest_row.is_some_and(|row| latest_tool.is_none_or(|tool| row > tool));
    if row_follows_tool {
        update_legacy_status(transcript, latest_row, update.status);
        return RowChange::Updated;
    }
    transcript.push(TranscriptItem::SubAgentSession {
        key: update.key,
        status: update.status,
        invocation_id: None,
        progress: None,
    });
    RowChange::Created
}

fn update_legacy_status(
    transcript: &mut [TranscriptItem],
    row: Option<usize>,
    status: SubAgentStatus,
) {
    let Some(TranscriptItem::SubAgentSession {
        status: row_status, ..
    }) = row.and_then(|row| transcript.get_mut(row))
    else {
        return;
    };
    *row_status = status;
}
