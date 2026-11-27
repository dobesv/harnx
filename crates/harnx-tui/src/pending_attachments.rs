//! Client-local attachment selection, preview and removal.

use crate::types::Tui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::Path;

const MAX_VISIBLE_PENDING_ATTACHMENTS: usize = 5;

#[derive(Default)]
pub(super) struct PendingAttachmentView {
    pub focus: Option<usize>,
    pub confirming_delete: bool,
}

// qual:allow(coupling, oi) reason: "Tui handlers live in focused modules; the shared frontend state is defined in types.rs"
impl Tui {
    /// Reset before moving or clearing draft files: automatic delivery of a
    /// queued message must close its preview/confirmation before the next draft.
    pub(super) fn reset_pending_attachment_view(&mut self) {
        if self.app.pending_attachment_view.focus.is_some() {
            self.close_pending_attachment_preview();
        }
        self.app.pending_attachment_view = PendingAttachmentView::default();
    }

    pub(super) fn pending_attachment_height(&self) -> u16 {
        if self.app.attachments.is_empty() {
            return 0;
        }
        // Keep the composer available even with many files. The list scrolls
        // with the selected attachment.
        1 + self
            .app
            .attachments
            .len()
            .min(MAX_VISIBLE_PENDING_ATTACHMENTS) as u16
    }

    pub(super) fn render_pending_attachments(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: ratatui::layout::Rect,
    ) {
        use ratatui::{
            style::{Color, Modifier, Style},
            text::Line,
            widgets::Paragraph,
        };
        if area.height == 0 {
            return;
        }
        let count = self.app.attachments.len();
        let visible = usize::from(area.height.saturating_sub(1));
        let selected = self.app.pending_attachment_view.focus;
        let start = selected
            .unwrap_or(count.saturating_sub(1))
            .saturating_sub(visible.saturating_sub(1));
        let hints = if selected.is_some() {
            "↑↓ select · Enter view · d remove · Esc back"
        } else {
            "↑ select · Ctrl+A attach"
        };
        let mut lines = vec![Line::from(format!(
            " Pending attachments ({count}) · {hints}"
        ))];
        for (index, attachment) in self
            .app
            .attachments
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
        {
            let line = Line::from(format!(
                " {} {}",
                if selected == Some(index) { "›" } else { " " },
                attachment.display_name
            ));
            lines.push(if selected == Some(index) {
                line.style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::REVERSED),
                )
            } else {
                line
            });
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    pub(super) fn focus_pending_from_input(&mut self) -> bool {
        if self.app.attachments.is_empty()
            || self.app.input.cursor().0 != 0
            || !self.app.completions.is_empty()
            || self.app.transcript_focus.is_some()
            || self.interaction_reading()
        {
            return false;
        }
        self.focus_pending_attachment(self.app.attachments.len() - 1);
        true
    }

    pub(super) fn focus_pending_attachment(&mut self, index: usize) {
        self.app.pending_attachment_view.focus = Some(index);
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = false;
        self.app.scroll_state.follow = true;
        self.app.completions.clear();
        self.refresh_input_chrome();
    }

    pub(super) async fn handle_pending_attachment_key(&mut self, key: KeyEvent) -> bool {
        let Some(index) = self.app.pending_attachment_view.focus else {
            return false;
        };
        if index >= self.app.attachments.len() {
            self.reset_pending_attachment_view();
            return true;
        }
        if key == KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL) {
            self.handle_ctrl_d().await;
            return true;
        }
        if key == KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL) {
            self.handle_ctrl_c().await;
            return true;
        }
        if self.app.pending_attachment_view.confirming_delete {
            self.confirm_pending_attachment_key(key, index).await;
            return true;
        }
        if self.app.detail_view_open {
            match (key.code, key.modifiers) {
                (KeyCode::Delete | KeyCode::Char('d'), KeyModifiers::NONE) => {
                    self.app.pending_attachment_view.confirming_delete = true;
                }
                (KeyCode::Esc, KeyModifiers::NONE) => self.close_pending_attachment_preview(),
                _ => {
                    self.handle_detail_scroll_key(key);
                }
            }
            return true;
        }
        self.navigate_pending_attachment(key, index).await;
        true
    }

    async fn navigate_pending_attachment(&mut self, key: KeyEvent, index: usize) {
        match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) if index > 0 => {
                self.app.pending_attachment_view.focus = Some(index - 1);
            }
            (KeyCode::Up, KeyModifiers::NONE) => {
                if self.enter_transcript_browsing() {
                    self.app.pending_attachment_view.focus = None;
                }
            }
            (KeyCode::Down, KeyModifiers::NONE) if index + 1 < self.app.attachments.len() => {
                self.app.pending_attachment_view.focus = Some(index + 1);
            }
            (KeyCode::Down | KeyCode::Esc, KeyModifiers::NONE) => {
                self.app.pending_attachment_view.focus = None;
                self.refresh_input_chrome();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => self.preview_pending_attachment(index).await,
            (KeyCode::Delete | KeyCode::Char('d'), KeyModifiers::NONE) => {
                self.app.pending_attachment_view.confirming_delete = true;
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.handle_ctrl_c().await,
            _ => {}
        }
    }

    async fn confirm_pending_attachment_key(&mut self, key: KeyEvent, index: usize) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('y' | 'Y'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                let name = self.app.attachments[index].display_name.clone();
                self.close_pending_attachment_preview();
                // Editing a locally queued draft must update its shared copy too.
                self.app.pending_message = None;
                self.clear_shared_pending_message().await;
                self.detach_named_attachment(&name);
                self.app.pending_attachment_view.focus = self
                    .app
                    .attachments
                    .len()
                    .checked_sub(1)
                    .map(|last| index.min(last));
                self.app.pending_attachment_view.confirming_delete = false;
            }
            (
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n' | 'N'),
                KeyModifiers::NONE | KeyModifiers::SHIFT,
            ) => {
                self.app.pending_attachment_view.confirming_delete = false;
            }
            _ => {}
        }
    }

    fn close_pending_attachment_preview(&mut self) {
        self.app.detail_view_open = false;
        self.app.detail_view_text = None;
        self.app.detail_view_title = None;
    }

    async fn preview_pending_attachment(&mut self, index: usize) {
        let attachment = &self.app.attachments[index];
        let content = pending_attachment_preview(&attachment.path).await;
        self.app.detail_view_title = Some(attachment.display_name.clone());
        self.app.detail_view_text = Some(content);
        self.app.detail_view_entry = None;
        self.app.doc_view = None;
        self.app.detail_view_scroll = ratatui_widget_scrolling::ScrollState::new();
        self.app.detail_view_scroll.follow = false;
        self.app.detail_view_open = true;
    }
}

async fn pending_attachment_preview(path: &Path) -> String {
    use tokio::io::AsyncReadExt;
    // Bound preview memory even when the user selects a very large file.
    const LIMIT: u64 = 256 * 1024;
    let Ok(file) = tokio::fs::File::open(path).await else {
        return "Unable to read attachment".into();
    };
    let size = file.metadata().await.map(|meta| meta.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    if file.take(LIMIT + 1).read_to_end(&mut bytes).await.is_err() {
        return "Unable to read attachment".into();
    }
    let truncated = bytes.len() > LIMIT as usize;
    bytes.truncate(LIMIT as usize);
    let text = match std::str::from_utf8(&bytes) {
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()])
        }
        result => result,
    };
    match text {
        Ok(text) => format!(
            "{size} bytes\n\n{}{}",
            crate::strip_ansi(text),
            if truncated {
                "\n[Preview truncated]"
            } else {
                ""
            }
        ),
        Err(_) => format!(
            "{size} bytes\nBinary attachment ({})",
            path.extension().unwrap_or_default().to_string_lossy()
        ),
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn truncated_preview_preserves_utf8_and_binary_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary.txt");
        let prefix = "a".repeat(256 * 1024 - 1);
        tokio::fs::write(&path, format!("{prefix}é")).await.unwrap();
        let preview = super::pending_attachment_preview(&path).await;
        assert!(preview.contains(&prefix));
        assert!(preview.ends_with("[Preview truncated]"));
        tokio::fs::write(&path, [0xff, 0xfe]).await.unwrap();
        assert!(super::pending_attachment_preview(&path)
            .await
            .contains("2 bytes\nBinary attachment"));
    }
}
