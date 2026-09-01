//! Navigation and transcript state for detached handoffs and nested sessions.

use crate::lifecycle::{
    session_history_transcript_items, subagent_key_from_output, subagent_progress_from_output,
};
use crate::subagent_transcript::{apply_child_event, flatten_subagent_event};
use crate::types::{
    MonitoredSessionKey, MonitoredSessionState, SubAgentStatus, SubAgentView, TranscriptItem, Tui,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::event::{AgentEvent, SessionEvent, ToolEvent, TurnEvent};

impl Tui {
    pub(super) async fn handle_session_event(
        &mut self,
        event: &AgentEvent,
        is_sub_agent: bool,
    ) -> bool {
        if self
            .handle_session_navigation_event(event, is_sub_agent)
            .await
        {
            return true;
        }
        self.handle_turn_activity(event, is_sub_agent).await
    }

    pub(super) fn open_focused_root_subagent(&mut self) -> bool {
        let Some(view) = self
            .app
            .transcript_focus
            .and_then(|focus| self.app.transcript.get(focus))
            .and_then(subagent_row_view)
        else {
            return false;
        };
        self.app.detail_view_open = false;
        self.app.detail_view_entry = None;
        self.app.subagent_view_stack.push(view);
        true
    }

    pub(super) fn open_focused_root_item(&mut self) {
        if !self.open_focused_root_subagent() {
            self.open_detail_view_for_focused_item();
        }
    }

    pub(super) fn handle_subagent_view_key(&mut self, key: KeyEvent) {
        let Some(current) = self
            .app
            .subagent_view_stack
            .last()
            .map(|view| view.key.clone())
        else {
            return;
        };
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            self.app.subagent_view_stack.pop();
            return;
        }
        if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
            self.open_focused_child_item(&current);
        } else if let Some(state) = self.app.monitored_sessions.get_mut(&current) {
            navigate_child_transcript(state, key);
        }
    }

    fn open_focused_child_item(&mut self, current: &MonitoredSessionKey) {
        let Some(item) = self
            .app
            .monitored_sessions
            .get(current)
            .and_then(|state| state.transcript_focus.map(|focus| (state, focus)))
            .and_then(|(state, focus)| state.transcript.get(focus))
            .cloned()
        else {
            return;
        };
        match item {
            TranscriptItem::SubAgentSession {
                key,
                status,
                progress,
                ..
            } => {
                self.app.subagent_view_stack.push(SubAgentView {
                    key,
                    status,
                    progress,
                });
            }
            entry => self.open_child_detail(entry),
        }
    }

    fn open_child_detail(&mut self, entry: TranscriptItem) {
        let mut scroll = ratatui_widget_scrolling::ScrollState::new();
        scroll.follow = false;
        self.app.detail_view_scroll = scroll;
        self.app.detail_view_text = None;
        self.app.detail_view_title = None;
        self.app.detail_view_entry = Some(entry);
        self.app.detail_view_open = true;
    }

    pub(super) fn scroll_open_subagent(&mut self, up: bool) -> bool {
        let Some(key) = self.app.subagent_view_stack.last().map(|view| &view.key) else {
            return false;
        };
        let Some(state) = self.app.monitored_sessions.get_mut(key) else {
            return true;
        };
        for _ in 0..3 {
            if up {
                state.scroll.scroll_up();
            } else {
                state.scroll.scroll_down();
            }
        }
        true
    }

    pub(super) fn current_session_cluster(&self) -> String {
        self.config
            .read()
            .remote_agent
            .as_ref()
            .map(|(_, cluster)| cluster.clone())
            .unwrap_or_else(|| harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string())
    }

    pub(super) fn handle_subagent_snapshot(
        &mut self,
        key: MonitoredSessionKey,
        transcript: Vec<TranscriptItem>,
        status: SubAgentStatus,
    ) {
        let nested = transcript
            .iter()
            .filter_map(subagent_row_key)
            .collect::<Vec<_>>();
        let state = self
            .app
            .monitored_sessions
            .entry(key.clone())
            .or_insert_with(|| MonitoredSessionState::new(status.clone()));
        state.transcript = transcript;
        state.status = status.clone();
        state.streaming_open = false;
        if state
            .transcript_focus
            .is_some_and(|focus| focus >= state.transcript.len())
        {
            state.transcript_focus = None;
        }
        self.update_subagent_row_status(&key, status);
        for nested_key in nested {
            self.ensure_subagent_monitor(nested_key);
        }
    }

    pub(super) fn handle_subagent_session_event(
        &mut self,
        key: MonitoredSessionKey,
        event: AgentEvent,
    ) {
        let event = flatten_subagent_event(event);
        if self.handle_nested_session_marker(&key, &event) {
            return;
        }

        let status_change = {
            let state = self
                .app
                .monitored_sessions
                .entry(key.clone())
                .or_insert_with(|| MonitoredSessionState::new(SubAgentStatus::Running));
            apply_child_event(state, event)
        };
        if let Some(status) = status_change {
            self.update_subagent_row_status(&key, status);
        }
    }

    pub(super) async fn handle_handoff_committed(&mut self, agent: String, session_id: String) {
        if agent.trim().is_empty() || session_id.trim().is_empty() {
            return;
        }
        let inherited_cluster = self.current_session_cluster();
        let (target_ref, target_cluster) = handoff_target(&agent, &inherited_cluster);
        if let Err(error) = harnx_runtime::config::Config::use_agent(
            &self.config,
            &target_ref,
            Some(&session_id),
            harnx_runtime::utils::create_abort_signal(),
        )
        .await
        {
            self.app.transcript.push(TranscriptItem::ErrorText(format!(
                "Handoff target '{agent}/{session_id}' was activated, but the TUI could not open it: {error:#}"
            )));
            self.pin_transcript_to_bottom();
            return;
        }

        self.current_prompt_abort = None;
        self.active_remote_session = Some((session_id, target_cluster));
        self.reset_for_handoff_target().await;
    }

    async fn handle_session_navigation_event(
        &mut self,
        event: &AgentEvent,
        is_sub_agent: bool,
    ) -> bool {
        if let AgentEvent::Session(SessionEvent::HandoffCommitted { agent, session_id }) = event {
            if !is_sub_agent {
                self.handle_handoff_committed(agent.clone(), session_id.clone())
                    .await;
                return true;
            }
        }
        self.handle_subagent_marker(None, event)
    }

    fn handle_subagent_marker(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        event: &AgentEvent,
    ) -> bool {
        let cluster = parent.map_or_else(
            || self.current_session_cluster(),
            |parent| parent.cluster.clone(),
        );
        match event {
            AgentEvent::Turn(TurnEvent::SubAgentStarted {
                agent,
                session_id,
                invocation_id,
            }) => {
                self.record_subagent_started(
                    parent,
                    MonitoredSessionKey {
                        agent: agent.clone(),
                        session_id: session_id.clone(),
                        cluster,
                    },
                    invocation_id.clone(),
                );
                true
            }
            AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)) => {
                self.record_subagent_progress(parent, progress.clone());
                true
            }
            AgentEvent::Tool(ToolEvent::Completed { output, .. }) => {
                let progress = subagent_progress_from_output(output);
                let Some(key) = subagent_key_from_output(output, &cluster) else {
                    if let Some(progress) = progress {
                        self.record_subagent_progress(parent, progress);
                        return true;
                    }
                    return false;
                };

                self.insert_subagent_reply(parent, &key, output);

                match progress {
                    Some(progress) => self.record_subagent_progress(parent, progress),
                    None => self.record_subagent_completed(parent, key),
                }
                true
            }
            _ => false,
        }
    }

    fn handle_nested_session_marker(
        &mut self,
        parent: &MonitoredSessionKey,
        event: &AgentEvent,
    ) -> bool {
        self.handle_subagent_marker(Some(parent), event)
    }

    async fn reset_for_handoff_target(&mut self) {
        self.app.llm_busy = true;
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.last_ui_output_source = None;
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = false;
        self.app.detail_view_open = false;
        self.app.detail_view_entry = None;
        self.app.transcript = session_history_transcript_items(&self.config).await;
        self.subagent_rows_dirty = true;
        self.pin_transcript_to_bottom();
        self.refresh_input_chrome();
        self.sync_session_activity_monitor();
    }
    /// Insert the sub-agent's final reply (from the tool result `response`
    /// field) as a `ToolResultMarkdown` row immediately before its
    /// `SubAgentSession` status row, keeping it adjacent to the originating
    /// `ToolCall`. No-op when the result carries no reply text.
    fn insert_subagent_reply(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        key: &MonitoredSessionKey,
        output: &serde_json::Value,
    ) {
        let Some(result_item) = crate::lifecycle::subagent_reply_item_from_output(output) else {
            return;
        };
        let progress = crate::lifecycle::subagent_progress_from_output(output);
        let invocation_id = progress.as_ref().map(|p| p.invocation_id.as_str());
        let transcript = self.subagent_transcript_mut(parent);
        insert_reply_before_status_row(transcript, key, invocation_id, result_item);
    }

    /// The transcript that owns `parent`'s sub-agent rows: the monitored
    /// child transcript for a nested parent, otherwise the main transcript.
    fn subagent_transcript_mut(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
    ) -> &mut Vec<TranscriptItem> {
        if let Some(parent) = parent {
            if let Some(session) = self.app.monitored_sessions.get_mut(parent) {
                return &mut session.transcript;
            }
        }
        &mut self.app.transcript
    }
}

fn navigate_child_transcript(state: &mut MonitoredSessionState, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) => {
            let start = state.transcript_focus.unwrap_or(state.transcript.len());
            let previous = (0..start)
                .rev()
                .find(|index| state.transcript[*index].is_navigable());
            if let Some(previous) = previous {
                state.transcript_focus = Some(previous);
                state.scroll_to_focused_item = true;
            }
            state.scroll.follow = false;
        }
        (KeyCode::Down, KeyModifiers::NONE) => {
            let start = state.transcript_focus.map_or(0, |focus| focus + 1);
            let next = (start..state.transcript.len())
                .find(|index| state.transcript[*index].is_navigable());
            if let Some(next) = next {
                state.transcript_focus = Some(next);
                state.scroll_to_focused_item = true;
            }
            state.scroll.follow = false;
        }
        (KeyCode::PageUp, KeyModifiers::NONE) => scroll_child(state, true, 10),
        (KeyCode::PageDown, KeyModifiers::NONE) => scroll_child(state, false, 10),
        _ => {}
    }
}

fn scroll_child(state: &mut MonitoredSessionState, up: bool, lines: usize) {
    for _ in 0..lines {
        if up {
            state.scroll.scroll_up();
        } else {
            state.scroll.scroll_down();
        }
    }
}

fn subagent_row_key(item: &TranscriptItem) -> Option<MonitoredSessionKey> {
    match item {
        TranscriptItem::SubAgentSession { key, .. } => Some(key.clone()),
        _ => None,
    }
}

fn subagent_row_view(item: &TranscriptItem) -> Option<SubAgentView> {
    match item {
        TranscriptItem::SubAgentSession {
            key,
            status,
            progress,
            ..
        } => Some(SubAgentView {
            key: key.clone(),
            status: status.clone(),
            progress: progress.clone(),
        }),
        _ => None,
    }
}

fn handoff_target(agent: &str, inherited_cluster: &str) -> (String, String) {
    match harnx_core::agent_ref::AgentRef::parse(agent) {
        harnx_core::agent_ref::AgentRef::Remote { cluster, .. } => {
            (agent.to_string(), cluster.into_owned())
        }
        harnx_core::agent_ref::AgentRef::Local(_)
            if inherited_cluster != harnx_runtime::config::LOCAL_CLUSTER_KEY =>
        {
            (
                format!("{agent}@{inherited_cluster}"),
                inherited_cluster.to_string(),
            )
        }
        harnx_core::agent_ref::AgentRef::Local(_) => {
            (agent.to_string(), inherited_cluster.to_string())
        }
    }
}

/// Place `result_item` right before the matched `SubAgentSession` row (falling
/// back to appending when no matching row exists, preserving the ToolCall ->
/// ToolResultMarkdown -> SubAgentSession ordering invariant so future maintainers
/// don't break detail-view pairing).
/// Skips insertion when an identical reply row is already present to keep
/// re-delivered terminal events idempotent.
fn insert_reply_before_status_row(
    transcript: &mut Vec<TranscriptItem>,
    key: &MonitoredSessionKey,
    invocation_id: Option<&str>,
    result_item: TranscriptItem,
) {
    let Some(pos) = transcript.iter().rposition(|item| {
        let TranscriptItem::SubAgentSession {
            key: row_key,
            invocation_id: row_inv_id,
            ..
        } = item
        else {
            return false;
        };
        if row_key != key {
            return false;
        }
        if let Some(inv_id) = invocation_id {
            row_inv_id.as_deref() == Some(inv_id)
        } else {
            true
        }
    }) else {
        transcript.push(result_item);
        return;
    };

    let already_has_result = pos > 0
        && match (&transcript[pos - 1], &result_item) {
            (
                TranscriptItem::ToolResultMarkdown { text: t1, .. },
                TranscriptItem::ToolResultMarkdown { text: t2, .. },
            ) => t1 == t2,
            _ => false,
        };

    if !already_has_result {
        transcript.insert(pos, result_item);
    }
}
