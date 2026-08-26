//! Navigation and transcript state for detached handoffs and nested sessions.

use crate::lifecycle::{session_history_transcript_items, subagent_key_from_output};
use crate::subagent_transcript::{apply_child_event, flatten_subagent_event, update_row};
use crate::types::{
    MonitoredSessionKey, MonitoredSessionState, SubAgentStatus, TranscriptItem, Tui,
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
        let Some(key) = self
            .app
            .transcript_focus
            .and_then(|focus| self.app.transcript.get(focus))
            .and_then(subagent_row_key)
        else {
            return false;
        };
        self.app.detail_view_open = false;
        self.app.detail_view_entry = None;
        self.app.subagent_view_stack.push(key);
        true
    }

    pub(super) fn open_focused_root_item(&mut self) {
        if !self.open_focused_root_subagent() {
            self.open_detail_view_for_focused_item();
        }
    }

    pub(super) fn handle_subagent_view_key(&mut self, key: KeyEvent) {
        let Some(current) = self.app.subagent_view_stack.last().cloned() else {
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
            TranscriptItem::SubAgentSession { key, .. } => {
                self.app.subagent_view_stack.push(key);
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
        let Some(key) = self.app.subagent_view_stack.last() else {
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

    pub(super) fn record_subagent_started(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        key: MonitoredSessionKey,
    ) {
        if key.agent.trim().is_empty() || key.session_id.trim().is_empty() {
            return;
        }
        self.upsert_subagent_row(parent, key.clone(), SubAgentStatus::Running);
        let state = self
            .app
            .monitored_sessions
            .entry(key.clone())
            .or_insert_with(|| MonitoredSessionState::new(SubAgentStatus::Running));
        state.status = SubAgentStatus::Running;
        state.streaming_open = false;
        self.update_subagent_row_status(&key, SubAgentStatus::Running);
        self.ensure_subagent_monitor(key);
        self.pin_transcript_to_bottom();
    }

    pub(super) fn record_subagent_completed(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        key: MonitoredSessionKey,
    ) {
        self.upsert_subagent_row(parent, key.clone(), SubAgentStatus::Completed);
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
        match event {
            AgentEvent::Session(SessionEvent::HandoffCommitted { agent, session_id })
                if !is_sub_agent =>
            {
                self.handle_handoff_committed(agent.clone(), session_id.clone())
                    .await;
                true
            }
            AgentEvent::Turn(TurnEvent::SubAgentStarted { agent, session_id }) => {
                self.record_subagent_started(
                    None,
                    MonitoredSessionKey {
                        agent: agent.clone(),
                        session_id: session_id.clone(),
                        cluster: self.current_session_cluster(),
                    },
                );
                true
            }
            AgentEvent::Tool(ToolEvent::Completed { output, .. }) => {
                let Some(key) = subagent_key_from_output(output, &self.current_session_cluster())
                else {
                    return false;
                };
                self.record_subagent_completed(None, key);
                true
            }
            _ => false,
        }
    }

    fn upsert_subagent_row(
        &mut self,
        parent: Option<&MonitoredSessionKey>,
        key: MonitoredSessionKey,
        status: SubAgentStatus,
    ) {
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
        let latest_tool = transcript
            .iter()
            .rposition(|item| matches!(item, TranscriptItem::ToolCall { .. }));
        let latest_row = transcript.iter().rposition(
            |item| matches!(item, TranscriptItem::SubAgentSession { key: row_key, .. } if row_key == &key),
        );
        if latest_row.is_some_and(|row| latest_tool.is_none_or(|tool| row > tool)) {
            if let Some(TranscriptItem::SubAgentSession {
                status: row_status, ..
            }) = latest_row.and_then(|row| transcript.get_mut(row))
            {
                *row_status = status;
            }
        } else {
            transcript.push(TranscriptItem::SubAgentSession { key, status });
        }
    }

    fn update_subagent_row_status(&mut self, key: &MonitoredSessionKey, status: SubAgentStatus) {
        update_row(&mut self.app.transcript, key, &status);
        for state in self.app.monitored_sessions.values_mut() {
            update_row(&mut state.transcript, key, &status);
        }
    }

    fn handle_nested_session_marker(
        &mut self,
        parent: &MonitoredSessionKey,
        event: &AgentEvent,
    ) -> bool {
        match event {
            AgentEvent::Turn(TurnEvent::SubAgentStarted { agent, session_id }) => {
                self.record_subagent_started(
                    Some(parent),
                    MonitoredSessionKey {
                        agent: agent.clone(),
                        session_id: session_id.clone(),
                        cluster: parent.cluster.clone(),
                    },
                );
                true
            }
            AgentEvent::Tool(ToolEvent::Completed { output, .. }) => {
                let Some(child) = subagent_key_from_output(output, &parent.cluster) else {
                    return false;
                };
                self.record_subagent_completed(Some(parent), child);
                true
            }
            _ => false,
        }
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
