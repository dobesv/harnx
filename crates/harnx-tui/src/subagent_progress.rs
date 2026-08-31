//! Per-invocation sub-agent progress state and transcript correlation.

use crate::subagent_transcript::update_row;
use crate::types::{
    MonitoredSessionKey, MonitoredSessionState, SubAgentInvocationProgress, SubAgentStatus,
    TranscriptItem, Tui,
};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};

struct RowUpdate {
    key: MonitoredSessionKey,
    status: SubAgentStatus,
    invocation_id: Option<String>,
    progress: Option<SubAgentInvocationProgress>,
}

impl Tui {
    pub(super) fn record_subagent_started(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        key: MonitoredSessionKey,
        invocation_id: Option<String>,
    ) {
        if key.agent.trim().is_empty() || key.session_id.trim().is_empty() {
            return;
        }
        let progress = invocation_id.as_ref().map(|invocation_id| {
            SubAgentInvocationProgress::new(SubAgentProgress {
                invocation_id: invocation_id.clone(),
                agent: key.agent.clone(),
                session_id: key.session_id.clone(),
                status: SubAgentProgressStatus::Running,
                elapsed_ms: 0,
                usage: CompletionTokenUsage::default(),
                tool_call_count: 0,
            })
        });
        if !self.upsert_subagent_row(
            parent,
            RowUpdate {
                key: key.clone(),
                status: SubAgentStatus::Running,
                invocation_id,
                progress,
            },
        ) {
            return;
        }
        let state = self
            .app
            .monitored_sessions
            .entry(key.clone())
            .or_insert_with(|| MonitoredSessionState::new(SubAgentStatus::Running));
        state.status = SubAgentStatus::Running;
        state.streaming_open = false;
        self.ensure_subagent_monitor(key);
        self.pin_transcript_to_bottom();
    }

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
        let status = SubAgentStatus::from_progress(snapshot.status);
        let progress = SubAgentInvocationProgress::new(snapshot);
        if !self.upsert_subagent_row(
            parent,
            RowUpdate {
                key: key.clone(),
                status: status.clone(),
                invocation_id: Some(invocation_id.clone()),
                progress: Some(progress.clone()),
            },
        ) {
            return;
        }
        self.app
            .monitored_sessions
            .entry(key.clone())
            .or_insert_with(|| MonitoredSessionState::new(status.clone()))
            .status = status.clone();
        for view in &mut self.app.subagent_view_stack {
            if view_matches_invocation(view.progress.as_ref(), &invocation_id) {
                view.status = status.clone();
                view.progress = Some(progress.clone());
            }
        }
        self.ensure_subagent_monitor(key);
        self.pin_transcript_to_bottom();
    }

    fn upsert_subagent_row(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        update: RowUpdate,
    ) -> bool {
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
) -> bool {
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
        if progress
            .as_ref()
            .is_some_and(|current| current.snapshot.status != SubAgentProgressStatus::Running)
        {
            return false;
        }
        *status = update.status;
        if update.progress.is_some() {
            *progress = update.progress;
        }
        return true;
    }
    transcript.push(TranscriptItem::SubAgentSession {
        key: update.key,
        status: update.status,
        invocation_id: Some(invocation_id),
        progress: update.progress,
    });
    true
}

fn upsert_legacy(transcript: &mut Vec<TranscriptItem>, update: RowUpdate) -> bool {
    let latest_tool = transcript
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::ToolCall { .. }));
    let latest_row = transcript.iter().rposition(
        |item| matches!(item, TranscriptItem::SubAgentSession { key, .. } if key == &update.key),
    );
    let row_follows_tool = latest_row.is_some_and(|row| latest_tool.is_none_or(|tool| row > tool));
    if row_follows_tool {
        update_legacy_status(transcript, latest_row, update.status);
        return true;
    }
    transcript.push(TranscriptItem::SubAgentSession {
        key: update.key,
        status: update.status,
        invocation_id: None,
        progress: None,
    });
    true
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
