//! Shared keyboard state and content helpers for transcript detail overlays.

use crate::types::{App, TranscriptItem, Tui};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};

impl Tui {
    pub(super) fn open_detail_view_for_focused_item(&mut self) {
        self.app.detail_view_scroll = passive_scroll_state();
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
        if self.app.detail_view_entry.is_some() {
            self.handle_passive_detail_view_key(key);
            return Ok(());
        }
        self.handle_root_detail_view_key(key).await
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

pub(super) fn detail_view_content(app: &App) -> (Vec<Vec<Line<'static>>>, String) {
    if let Some(text) = &app.detail_view_text {
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
    if app.detail_view_entry.is_some() {
        " ↑↓/scroll  PgUp/PgDn/scroll  ESC/back".to_string()
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
