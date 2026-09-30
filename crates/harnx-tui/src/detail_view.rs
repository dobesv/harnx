//! Shared keyboard state and content helpers for transcript detail overlays.

use crate::subagent_sessions::CidOpenResult;
use crate::types::{App, TranscriptItem, Tui};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};

impl Tui {
    pub(super) fn open_detail_view_for_focused_item(&mut self) {
        self.app.detail_view_scroll = passive_scroll_state();
        self.app.doc_view = None;
        self.app.doc_history.clear();
        self.app.detail_view_entry = None;
        let focused_item = self
            .app
            .transcript_focus
            .and_then(|focus| self.app.transcript.get(focus));
        match focused_item {
            Some(TranscriptItem::CompactionMarker { detail_text, .. }) => {
                self.app.detail_view_text = Some(detail_text.clone());
                self.app.detail_view_title = Some("Compacted session".to_string());
            }
            _ => {
                self.app.detail_view_text = None;
                self.app.detail_view_title = None;
            }
        }
        self.app.detail_view_open = true;
    }

    pub(super) async fn handle_detail_view_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.app.doc_view.is_some() {
            self.handle_cid_document_key(key).await;
            return Ok(());
        }
        if self.app.detail_view_entry.is_some() {
            self.handle_passive_detail_view_key(key);
            return Ok(());
        }
        self.handle_root_detail_view_key(key).await
    }

    async fn handle_cid_document_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Tab | KeyCode::Down, KeyModifiers::NONE) => self.focus_next_doc_link(),
            (KeyCode::BackTab | KeyCode::Up, KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.focus_previous_doc_link();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => self.open_focused_doc_link().await,
            (KeyCode::Esc | KeyCode::Backspace, KeyModifiers::NONE) => {
                self.go_back_from_cid_document().await;
            }
            _ => {
                self.handle_detail_scroll_key(key);
            }
        }
    }

    fn focus_next_doc_link(&mut self) {
        let Some(view) = self.app.doc_view.as_mut() else {
            return;
        };
        view.focused_link = next_link(view.focused_link, view.links.len());
    }

    fn focus_previous_doc_link(&mut self) {
        let Some(view) = self.app.doc_view.as_mut() else {
            return;
        };
        view.focused_link = previous_link(view.focused_link, view.links.len());
    }

    async fn open_focused_doc_link(&mut self) {
        let Some((url, history_entry)) = focused_doc_link(&self.app) else {
            return;
        };
        if url.starts_with("http://") || url.starts_with("https://") {
            let _ = self.open_detached(std::path::Path::new(&url));
            return;
        }
        if !url.starts_with("cid:") {
            return;
        }
        self.app.doc_history.push(history_entry);
        match self.load_cid_url(&url).await {
            Ok(CidOpenResult::Document(view)) => self.show_cid_document(view),
            Ok(CidOpenResult::External) => {
                self.app.doc_history.pop();
            }
            Err(error) => {
                self.app.doc_history.pop();
                self.push_doc_resolve_error(&url, &error);
            }
        }
    }

    async fn go_back_from_cid_document(&mut self) {
        let Some((url, focused_link)) = self.app.doc_history.pop() else {
            self.app.detail_view_open = false;
            self.app.doc_view = None;
            return;
        };
        match self.load_cid_url(&url).await {
            Ok(CidOpenResult::Document(mut view)) => {
                view.focused_link = valid_focus(focused_link, view.links.len());
                self.show_cid_document(view);
            }
            Ok(CidOpenResult::External) => {
                if self.app.doc_history.is_empty() {
                    self.app.detail_view_open = false;
                    self.app.doc_view = None;
                }
            }
            Err(error) => {
                self.push_doc_resolve_error(&url, &error);
                if self.app.doc_history.is_empty() {
                    self.app.detail_view_open = false;
                    self.app.doc_view = None;
                }
            }
        }
    }

    fn push_doc_resolve_error(&mut self, url: &str, error: &anyhow::Error) {
        self.app.transcript.push(TranscriptItem::StatusLine(format!(
            "Failed to resolve {url}: {error:#}"
        )));
    }

    fn handle_passive_detail_view_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            self.app.detail_view_open = false;
            self.app.detail_view_entry = None;
        } else {
            self.handle_detail_scroll_key(key);
        }
    }

    async fn handle_root_detail_view_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.handle_detail_scroll_key(key) {
            return Ok(());
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => self.app.detail_view_open = false,
            (KeyCode::Char('e'), KeyModifiers::NONE) => self.edit_root_detail().await?,
            (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('d'), KeyModifiers::NONE) => {
                self.handle_transcript_delete();
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => self.handle_transcript_rewind(),
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                self.handle_transcript_copy();
                self.app.copy_notice_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_detail_scroll_key(&mut self, key: KeyEvent) -> bool {
        match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.app.detail_view_scroll.scroll_up();
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.app.detail_view_scroll.scroll_down();
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => scroll_detail(&mut self.app, true),
            (KeyCode::PageDown, KeyModifiers::NONE) => scroll_detail(&mut self.app, false),
            (
                KeyCode::Char('g' | '<') | KeyCode::Home,
                KeyModifiers::NONE | KeyModifiers::SHIFT,
            ) => {
                self.app.detail_view_scroll.scroll_to_top();
            }
            (KeyCode::Char('G' | '>') | KeyCode::End, KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.app.detail_view_scroll.scroll_to_bottom();
            }
            _ => return false,
        }
        true
    }

    async fn edit_root_detail(&mut self) -> Result<()> {
        let had_focus = self.app.transcript_focus;
        let prior_browsing = self.app.transcript_browsing;
        self.app.detail_view_open = false;
        self.handle_transcript_edit().await?;
        let Some(focus) = had_focus.filter(|focus| *focus < self.app.transcript.len()) else {
            return Ok(());
        };
        self.app.transcript_focus = Some(focus);
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = prior_browsing;
        self.open_detail_view_for_focused_item();
        Ok(())
    }
}

fn next_link(current: Option<usize>, link_count: usize) -> Option<usize> {
    if link_count == 0 {
        None
    } else {
        Some(current.map_or(0, |index| (index + 1) % link_count))
    }
}

fn previous_link(current: Option<usize>, link_count: usize) -> Option<usize> {
    if link_count == 0 {
        None
    } else {
        Some(current.map_or(link_count - 1, |index| {
            index.checked_sub(1).unwrap_or(link_count - 1)
        }))
    }
}

fn focused_doc_link(app: &App) -> Option<(String, (String, Option<usize>))> {
    let view = app.doc_view.as_ref()?;
    let link = view.links.get(view.focused_link?)?;
    Some((link.url.clone(), (view.url.clone(), view.focused_link)))
}

fn valid_focus(focused: Option<usize>, link_count: usize) -> Option<usize> {
    if link_count == 0 {
        None
    } else {
        focused.filter(|index| *index < link_count).or(Some(0))
    }
}

pub(super) fn detail_view_content(app: &App) -> (Vec<Vec<Line<'static>>>, String) {
    if let Some(view) = &app.doc_view {
        let entries = vec![view
            .text
            .lines()
            .map(|line| Line::from(Span::raw(line.to_string())))
            .collect()];
        (entries, view.title.clone())
    } else if let Some(text) = &app.detail_view_text {
        let entries = vec![text
            .lines()
            .map(|line| Line::from(Span::raw(line.to_string())))
            .collect()];
        let title = app
            .detail_view_title
            .clone()
            .unwrap_or_else(|| "Detail".to_string());
        (entries, title)
    } else if let Some(entry) = &app.detail_view_entry {
        (vec![Tui::render_entry_detail(entry)], "Detail".to_string())
    } else {
        selected_transcript_detail_content(app)
    }
}

pub(super) fn detail_view_footer_text(app: &App) -> String {
    if app.doc_view.is_some() {
        " ↑↓/Tab: select link  Enter: open  Backspace/Esc: back  PgUp/PgDn: scroll".to_string()
    } else if app.detail_view_entry.is_some() {
        " ↑↓/scroll  PgUp/PgDn/scroll  g/G top/bot  ESC/back".to_string()
    } else if app
        .copy_notice_until
        .is_some_and(|deadline| std::time::Instant::now() < deadline)
    {
        " ✓ Copied to clipboard".to_string()
    } else {
        " ↑↓/scroll  e/edit  d/delete  r/rewind  c/copy  ESC/back".to_string()
    }
}

fn passive_scroll_state() -> ratatui_widget_scrolling::ScrollState {
    let mut scroll = ratatui_widget_scrolling::ScrollState::new();
    scroll.follow = false;
    scroll
}

fn scroll_detail(app: &mut App, up: bool) {
    for _ in 0..10 {
        if up {
            app.detail_view_scroll.scroll_up();
        } else {
            app.detail_view_scroll.scroll_down();
        }
    }
}

fn selected_transcript_detail_content(app: &App) -> (Vec<Vec<Line<'static>>>, String) {
    let (from, to) = app.selected_transcript_range();
    let mut entries = Vec::new();
    for index in from..=to {
        let Some(entry) = app.transcript.get(index) else {
            continue;
        };
        entries.push(Tui::render_entry_detail(entry));
        append_paired_tool_result(app, index, to, &mut entries);
        if index < to {
            entries.push(vec![Line::from("")]);
        }
    }
    let title = if from == to {
        "Detail".to_string()
    } else {
        format!("Detail ({from}–{to})")
    };
    (entries, title)
}

fn append_paired_tool_result(
    app: &App,
    index: usize,
    selection_end: usize,
    entries: &mut Vec<Vec<Line<'static>>>,
) {
    let Some(entry) = app.transcript.get(index) else {
        return;
    };
    if !matches!(entry, TranscriptItem::ToolCall { .. }) || index < selection_end {
        return;
    }
    let Some(next) = app.transcript.get(index + 1) else {
        return;
    };
    if matches!(next, TranscriptItem::ToolResultMarkdown { .. }) {
        entries.push(vec![Line::from("")]);
        entries.push(Tui::render_entry_detail(next));
    }
}
