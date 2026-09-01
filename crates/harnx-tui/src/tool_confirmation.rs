//! State transitions for interactive tool-use confirmation modals.

use crate::types::{ModalState, ToolConfirmationEvent, TranscriptItem, Tui};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

fn confirmation_header(tool_name: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("Allow tool '{tool_name}'?"),
        Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::BOLD),
    ))
}

fn confirmation_parts(modal: &ModalState) -> Option<(&str, &str, Option<&str>)> {
    match modal {
        ModalState::ConfirmToolUse {
            tool_name,
            input_preview,
            reason,
        } => Some((tool_name, input_preview, reason.as_deref())),
        _ => None,
    }
}

impl Tui {
    pub(super) fn handle_tool_confirmation_event(&mut self, event: ToolConfirmationEvent) {
        match event {
            ToolConfirmationEvent::Show {
                confirmation_id,
                tool_name,
                input_preview,
                reason,
                reply,
            } => self.show_tool_confirmation(
                confirmation_id,
                tool_name,
                input_preview,
                reason,
                reply,
            ),
            ToolConfirmationEvent::Dismiss { confirmation_id } => {
                self.dismiss_tool_confirmation(confirmation_id);
            }
        }
    }

    fn show_tool_confirmation(
        &mut self,
        confirmation_id: u64,
        tool_name: String,
        input_preview: String,
        reason: Option<String>,
        reply: std::sync::mpsc::Sender<bool>,
    ) {
        if self.app.modal.is_some() || self.app.pending_confirm_reply.is_some() {
            let _ = reply.send(false);
            self.app.transcript.push(TranscriptItem::SystemText(
                "⚠ Tool confirmation denied because another modal is already open.".to_string(),
            ));
            self.pin_transcript_to_bottom();
            return;
        }

        // A blocked tool-eval thread is waiting on `reply`. Show the native
        // modal and remember the channel; answering the modal sends the
        // decision back.
        self.app.pending_confirm_reply = Some(reply);
        self.app.pending_confirm_id = Some(confirmation_id);
        self.app.modal = Some(ModalState::ConfirmToolUse {
            tool_name,
            input_preview,
            reason,
        });
    }

    fn dismiss_tool_confirmation(&mut self, confirmation_id: u64) {
        if self.app.pending_confirm_id != Some(confirmation_id) {
            return;
        }
        self.resolve_tool_confirm(false);
        self.app.transcript.push(TranscriptItem::SystemText(
            "⚠ Tool confirmation expired or was cancelled.".to_string(),
        ));
        self.pin_transcript_to_bottom();
    }

    /// Resolve an in-flight tool-use confirmation: send the decision to the
    /// blocked tool-eval thread and dismiss the modal.
    pub(super) fn resolve_tool_confirm(&mut self, allow: bool) {
        if let Some(reply) = self.app.pending_confirm_reply.take() {
            let _ = reply.send(allow);
        }
        self.app.pending_confirm_id = None;
        self.app.modal = None;
    }

    pub(super) fn render_tool_confirm_overlay(
        &self,
        frame: &mut Frame<'_>,
        screen_size: ratatui::layout::Rect,
        modal: &ModalState,
    ) {
        let max_height = (screen_size.height / 2).max(6);
        let height = self
            .confirm_tool_modal_height(screen_size.width, modal)
            .clamp(6, max_height);
        let area = ratatui::layout::Rect::new(
            screen_size.x,
            screen_size.y + screen_size.height.saturating_sub(height),
            screen_size.width,
            height,
        );
        self.render_tool_confirm_modal(frame, area, modal);
    }

    pub(super) fn confirm_tool_modal_height(&self, width: u16, modal: &ModalState) -> u16 {
        let Some((_, input_preview, reason)) = confirmation_parts(modal) else {
            return 0;
        };
        let content_width = width.saturating_sub(2);
        if content_width == 0 {
            return 6;
        }
        let mut h: u16 = 6; // Borders(2) + Title(1) + OptionLine(1) + BlankLines(2)
        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            let entry = crate::markdown_render::render_markdown(
                r,
                Style::default(),
                content_width,
                self.code_theme.as_ref(),
            );
            h = h.saturating_add(entry.total_height.saturating_add(1)); // +1 for "Reason:" line
        }
        if !input_preview.is_empty() {
            let md = crate::render::fenced_json_markdown(input_preview);
            let entry = crate::markdown_render::render_markdown(
                &md,
                Style::default(),
                content_width,
                self.code_theme.as_ref(),
            );
            h = h.saturating_add(entry.total_height.saturating_add(1)); // +1 for "Input:" line
        }
        h
    }

    /// Multi-line confirmation modal for a `PreToolUse` "ask" gate. Shows the
    /// tool name, optional reason, optional argument preview, and a [y/N] prompt.
    pub(super) fn render_tool_confirm_modal(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        modal: &ModalState,
    ) {
        frame.render_widget(ratatui::widgets::Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                "Tool confirmation",
                Style::default().fg(Color::Yellow),
            ))
            .border_style(Style::default().fg(Color::Yellow));

        let inner_area = block.inner(area);
        frame.render_widget(block, area);

        if inner_area.height == 0 || inner_area.width == 0 {
            return;
        }

        let footer = Line::from(Span::styled(
            "[y] allow   [n] deny   (Enter/Esc denies)",
            Style::default().fg(Color::DarkGray),
        ));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),    // Scrollable content area
                Constraint::Length(1), // Footer area
            ])
            .split(inner_area);

        let content_area = chunks[0];
        let footer_area = chunks[1];

        // Draw footer
        frame.render_widget(Paragraph::new(footer), footer_area);

        if content_area.height == 0 {
            return;
        }

        self.render_tool_confirm_content(frame, content_area, modal);
    }

    fn render_tool_confirm_content(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        modal: &ModalState,
    ) {
        let Some((tool_name, input_preview, reason)) = confirmation_parts(modal) else {
            return;
        };
        let (dim, header) = (
            Style::default().fg(Color::DarkGray),
            confirmation_header(tool_name),
        );
        let (mut y, max_y) = (area.y, area.y + area.height);

        macro_rules! render_item {
            ($h:expr, $draw:expr) => {
                if y < max_y {
                    let h = ($h).min(max_y - y);
                    let rect = ratatui::layout::Rect::new(area.x, y, area.width, h);
                    $draw(rect, frame.buffer_mut());
                    y += $h;
                }
            };
        }

        render_item!(1, |rect, buffer| {
            ratatui::widgets::Widget::render(Paragraph::new(header.clone()), rect, buffer);
        });
        y += 1; // blank line

        if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
            render_item!(1, |rect, buffer| {
                ratatui::widgets::Widget::render(
                    Paragraph::new(Line::from(Span::styled("Reason: ", dim))),
                    rect,
                    buffer,
                );
            });
            let entry = crate::markdown_render::render_markdown(
                reason,
                Style::default(),
                area.width,
                self.code_theme.as_ref(),
            );
            render_item!(entry.total_height, |rect, buffer| {
                ratatui::widgets::Widget::render(entry.clone(), rect, buffer);
            });
        }

        if !input_preview.is_empty() {
            render_item!(1, |rect, buffer| {
                ratatui::widgets::Widget::render(
                    Paragraph::new(Line::from(Span::styled("Input: ", dim))),
                    rect,
                    buffer,
                );
            });
            let markdown = crate::render::fenced_json_markdown(input_preview);
            let entry = crate::markdown_render::render_markdown(
                &markdown,
                Style::default(),
                area.width,
                self.code_theme.as_ref(),
            );
            render_item!(entry.total_height, |rect, buffer| {
                ratatui::widgets::Widget::render(entry.clone(), rect, buffer);
            });
        }
        debug_assert!(y >= area.y);
    }
}
