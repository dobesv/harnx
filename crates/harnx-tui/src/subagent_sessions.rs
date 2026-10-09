//! Navigation and transcript state for detached handoffs and nested sessions.

use crate::lifecycle::{
    session_history_transcript_items, subagent_key_from_output, subagent_progress_from_output,
};
use crate::subagent_transcript::{apply_child_event, flatten_subagent_event};
use crate::types::{
    CidDocView, MonitoredSessionKey, MonitoredSessionState, SubAgentStatus, SubAgentView,
    TranscriptItem, Tui,
};
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::event::{AgentEvent, SessionEvent, ToolEvent, TurnEvent};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone)]
enum LinkTarget {
    Root,
    Child(MonitoredSessionKey),
}

pub(super) enum CidOpenResult {
    Document(CidDocView),
    External,
}

impl Tui {
    pub(super) fn cancel_selected_child(&mut self) -> bool {
        let view = self.app.subagent_view_stack.last().cloned().or_else(|| {
            self.app
                .transcript_focus
                .and_then(|focus| self.app.transcript.get(focus))
                .and_then(subagent_row_view)
        });
        let Some(view) = view else {
            return false;
        };
        if !matches!(
            view.status,
            SubAgentStatus::Running | SubAgentStatus::Unconfirmed
        ) {
            return !self.app.subagent_view_stack.is_empty();
        }
        let Some(progress) = view.progress else {
            return true;
        };
        if !self
            .app
            .monitored_sessions
            .get(&view.key)
            .is_some_and(|state| {
                state.execution_id.as_deref() == Some(&progress.snapshot.invocation_id)
            })
        {
            return true;
        }
        self.exit_after_cancel = false;
        self.start_cancellation(view.key.storage_key(), view.key.cluster);
        true
    }

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
        self.app.doc_view = None;
        self.app.doc_history.clear();
        self.app.detail_view_entry = None;
        self.app.subagent_view_stack.push(view);
        true
    }

    pub(super) async fn open_focused_root_item(&mut self) {
        if let Some(url) = self.focused_root_link() {
            self.open_link(&url, LinkTarget::Root).await;
            return;
        }
        if !self.open_focused_root_subagent() {
            self.open_detail_view_for_focused_item();
        }
    }

    fn focused_root_link(&self) -> Option<String> {
        let focus = self.app.transcript_focus?;
        let TranscriptItem::MarkdownLink { url, .. } = self.app.transcript.get(focus)? else {
            return None;
        };
        Some(url.clone())
    }

    pub(super) async fn handle_subagent_view_key(&mut self, key: KeyEvent) {
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
            self.open_focused_child_item(&current).await;
        } else if let Some(state) = self.app.monitored_sessions.get_mut(&current) {
            navigate_child_transcript(state, key);
        }
    }

    async fn open_focused_child_item(&mut self, current: &MonitoredSessionKey) {
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
            TranscriptItem::MarkdownLink { url, .. } => {
                self.open_link(&url, LinkTarget::Child(current.clone()))
                    .await;
            }
            entry => self.open_child_detail(entry),
        }
    }

    fn open_child_detail(&mut self, entry: TranscriptItem) {
        let mut scroll = ratatui_widget_scrolling::ScrollState::new();
        scroll.follow = false;
        self.app.detail_view_scroll = scroll;
        self.app.doc_view = None;
        self.app.doc_history.clear();
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
        let config = self.config.read();
        config
            .remote_agent
            .as_ref()
            .map(|(_, cluster)| cluster.clone())
            .unwrap_or_else(|| config.default_cluster_key().to_string())
    }

    pub(super) fn handle_subagent_snapshot(
        &mut self,
        key: MonitoredSessionKey,
        snapshot: crate::types::SubAgentSnapshot,
    ) {
        let crate::types::SubAgentSnapshot {
            invocation_id,
            transcript,
            status,
        } = snapshot;
        if self
            .app
            .monitored_sessions
            .get(&key)
            .and_then(|state| state.invocation_id.as_ref())
            != invocation_id.as_ref()
        {
            return;
        }
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
        state.streamed_text_idx = None;
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
        stamp: crate::event_isolation::EventStamp,
        event: AgentEvent,
    ) {
        if !self
            .app
            .monitored_sessions
            .get(&key)
            .is_some_and(|state| stamp.allows(&state.live_events))
        {
            return;
        }
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
        let (target_ref, _) = handoff_target(&agent, &inherited_cluster);
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
        self.active_remote_session = self.session_activity_destination();
        self.reset_for_handoff_target().await;
    }

    async fn handle_session_navigation_event(
        &mut self,
        event: &AgentEvent,
        is_sub_agent: bool,
    ) -> bool {
        if let AgentEvent::Session(SessionEvent::HandoffCommitted {
            agent, session_id, ..
        }) = event
        {
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
        self.app.streamed_text_idx = None;
        self.app.last_ui_output_source = None;
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = false;
        self.app.detail_view_open = false;
        self.app.doc_view = None;
        self.app.doc_history.clear();
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
        let Some(result_item) = crate::lifecycle::subagent_reply_item_from_output(output, key)
        else {
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

    async fn open_link(&mut self, url: &str, target: LinkTarget) {
        if url.starts_with("http://") || url.starts_with("https://") {
            if let Err(error) = self.open_detached(Path::new(url)) {
                self.push_link_status(&target, format!("Failed to open {url}: {error:#}"));
            }
            return;
        }
        if !url.starts_with("cid:") {
            return;
        }
        match self.load_cid_url(url).await {
            Ok(CidOpenResult::Document(view)) => {
                self.app.doc_history.clear();
                self.show_cid_document(view);
            }
            Ok(CidOpenResult::External) => {}
            Err(error) => {
                self.push_link_status(&target, format!("Failed to resolve {url}: {error:#}"));
            }
        }
    }

    pub(super) async fn load_cid_url(&self, url: &str) -> Result<CidOpenResult> {
        let resolved = self.resolve_cid_blob(url).await?;
        if is_text_mime(&resolved.mime_type) {
            let text =
                String::from_utf8(resolved.bytes).context("resolved document is not UTF-8")?;
            return Ok(CidOpenResult::Document(cid_document(url, text)));
        }
        let path = write_open_temp_file(&resolved.bytes, &resolved.mime_type)?;
        self.open_detached(&path)
            .with_context(|| format!("open attachment temp file '{}'", path.display()))?;
        Ok(CidOpenResult::External)
    }

    async fn resolve_cid_blob(&self, url: &str) -> Result<harnx_blob_store::ResolvedBlob> {
        #[cfg(test)]
        if let Some(resolve) = &self.cid_resolve_override {
            return resolve(url.to_string()).await;
        }
        let cid_url = harnx_core::cid_url::CidUrl::parse(url).context("parse cid: URL")?;
        let config = self.config.read().clone();
        let cluster = config.default_cluster_key().to_string();
        let jetstream = config.nats_jetstream(&cluster).await?;
        harnx_blob_store::resolve(&jetstream, &cid_url).await
    }

    pub(super) fn show_cid_document(&mut self, view: CidDocView) {
        self.app.detail_view_scroll = passive_scroll_state();
        self.app.detail_view_text = None;
        self.app.detail_view_title = None;
        self.app.detail_view_entry = None;
        self.app.doc_view = Some(view);
        self.app.detail_view_open = true;
    }

    pub(super) fn open_detached(&self, target: &Path) -> Result<()> {
        #[cfg(test)]
        if let Some(open) = &self.detached_open_override {
            return open(target);
        }
        open::that_detached(target).context("launch system opener")
    }

    fn push_link_status(&mut self, target: &LinkTarget, message: String) {
        let transcript = match target {
            LinkTarget::Root => &mut self.app.transcript,
            LinkTarget::Child(key) => match self.app.monitored_sessions.get_mut(key) {
                Some(state) => &mut state.transcript,
                None => &mut self.app.transcript,
            },
        };
        transcript.push(TranscriptItem::StatusLine(message));
        if matches!(target, LinkTarget::Root) {
            self.pin_transcript_to_bottom();
        }
    }
}

fn passive_scroll_state() -> ratatui_widget_scrolling::ScrollState {
    let mut scroll = ratatui_widget_scrolling::ScrollState::new();
    scroll.follow = false;
    scroll
}

fn cid_document(url: &str, text: String) -> CidDocView {
    let links = crate::markdown_render::extract_markdown_links(&text);
    let focused_link = (!links.is_empty()).then_some(0);
    CidDocView {
        url: url.to_string(),
        title: document_title(url, &text),
        text,
        links,
        focused_link,
    }
}

fn document_title(url: &str, text: &str) -> String {
    text.lines()
        .find_map(|line| {
            let trimmed = line.trim_start();
            let heading = trimmed.strip_prefix('#')?.trim_start_matches('#').trim();
            (!heading.is_empty()).then(|| heading.to_string())
        })
        .unwrap_or_else(|| url.rsplit('/').next().unwrap_or(url).to_string())
}

pub(super) fn write_open_temp_file(bytes: &[u8], mime_type: &str) -> Result<PathBuf> {
    let suffix = format!(".{}", extension_for_mime(mime_type));
    let mut file = tempfile::Builder::new()
        .prefix("harnx-attachment-")
        .suffix(&suffix)
        .tempfile()
        .context("create attachment temp file")?;
    file.write_all(bytes)
        .context("write attachment temp file")?;
    file.flush().context("flush attachment temp file")?;
    let (_, path) = file
        .keep()
        .map_err(|error| error.error)
        .context("persist attachment temp file")?;
    Ok(path)
}

pub(super) fn extension_for_mime(mime_type: &str) -> &'static str {
    let mime_type = base_mime_type(mime_type).to_ascii_lowercase();
    if mime_type == "text/plain" {
        return "txt";
    }
    mime_guess::get_mime_extensions_str(&mime_type)
        .and_then(|extensions| extensions.first())
        .copied()
        .filter(|extension| !is_unsafe_extension(extension))
        .unwrap_or("bin")
}

pub(super) fn is_unsafe_extension(extension: &str) -> bool {
    matches!(
        extension,
        "application"
            | "bat"
            | "cmd"
            | "com"
            | "exe"
            | "hta"
            | "lnk"
            | "vbs"
            | "vbe"
            | "js"
            | "jse"
            | "wsf"
            | "wsh"
            | "scr"
            | "ps1"
            | "sh"
            | "bash"
            | "svg"
    )
}

fn is_text_mime(mime_type: &str) -> bool {
    let mime_type = base_mime_type(mime_type).to_ascii_lowercase();
    mime_type.starts_with("text/")
        || matches!(
            mime_type.as_str(),
            "application/json"
                | "application/javascript"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/sql"
                | "application/graphql"
        )
}

fn base_mime_type(mime_type: &str) -> &str {
    mime_type.split(';').next().unwrap_or_default().trim()
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
        (KeyCode::Char('g' | '<') | KeyCode::Home, KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            state.scroll.scroll_to_top();
        }
        (KeyCode::Char('G' | '>') | KeyCode::End, KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            state.scroll.scroll_to_bottom();
        }
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

/// Insert the reply and its links as one block before the invocation's status
/// row, or append when the status hasn't arrived. Re-delivery replaces the same
/// reply block, preserving ToolCall -> MarkdownLink* -> ToolResultMarkdown ->
/// MarkdownLink* -> SubAgentSession ordering and detail-view pairing.
fn insert_reply_before_status_row(
    transcript: &mut Vec<TranscriptItem>,
    key: &MonitoredSessionKey,
    invocation_id: Option<&str>,
    result_item: TranscriptItem,
) {
    let pos = transcript
        .iter()
        .rposition(|item| {
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
        })
        .unwrap_or(transcript.len());

    let mut links = Vec::new();
    if let TranscriptItem::ToolResultMarkdown { text, .. } = &result_item {
        crate::lifecycle::append_markdown_links(&mut links, text);
    }
    // Only a reply tagged with this identity may be replaced. Text equality
    // would confuse an adjacent ordinary result with the subagent's reply.
    let owner = (key.clone(), invocation_id.map(str::to_string));
    let existing = transcript[..pos].iter().rposition(|item| {
        matches!(
            item,
            TranscriptItem::ToolResultMarkdown { subagent_reply_owner: Some(reply_owner), .. }
                if reply_owner == &owner
        )
    });
    let start = existing.unwrap_or(pos);
    let end = existing.map_or(pos, |index| {
        index
            + 1
            + transcript[index + 1..pos]
                .iter()
                .take_while(|item| matches!(item, TranscriptItem::MarkdownLink { .. }))
                .count()
    });
    transcript.splice(start..end, std::iter::once(result_item).chain(links));
}
