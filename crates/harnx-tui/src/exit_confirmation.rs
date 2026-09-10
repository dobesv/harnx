//! Rendering for the active-turn exit confirmation tray.

use crate::types::{ExitPhase, ExitWorkerState, ModalState, Tui};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

fn confirmation_parts(modal: &ModalState) -> Option<(ExitWorkerState, ExitPhase)> {
    match modal {
        ModalState::ConfirmExit {
            worker_state,
            phase,
        } => Some((*worker_state, *phase)),
        _ => None,
    }
}

pub(crate) fn exit_body_copy(state: ExitWorkerState) -> &'static str {
    match state {
        ExitWorkerState::Remote => "Runs on a remote worker. Exit without interrupting and it keeps running there; reopening the session resumes it.",
        ExitWorkerState::LocalOwnedHere => "Runs on a local worker owned by this client. Exit without interrupting and the work stops; reopening the session resumes it from where it stopped.",
        ExitWorkerState::LocalOwnedElsewhere => "Runs on a local worker owned by another client. Exit without interrupting and it keeps running there; reopening the session resumes it.",
        ExitWorkerState::Unknown => "May keep running after you exit. If still in progress when you reopen, it resumes.",
    }
}

fn exit_action_copy(phase: ExitPhase) -> &'static str {
    match phase {
        ExitPhase::Prompting => {
            "[Ctrl+D] exit without interrupting   [Ctrl+C] interrupt and exit   [Esc] stay"
        }
        ExitPhase::Interrupting => {
            "Requesting cancellation…   [Esc] stay   [Ctrl+D] exit immediately"
        }
        ExitPhase::RequestFailed => {
            "Cancellation request failed. [R] retry   [Esc] stay   [Ctrl+D] exit immediately"
        }
    }
}

impl Tui {
    pub(super) fn exit_confirm_modal_height(&self, content_width: u16, modal: &ModalState) -> u16 {
        let Some((worker_state, phase)) = confirmation_parts(modal) else {
            return 0;
        };
        let content_width = content_width.max(1) as usize;
        let body_height = textwrap::wrap(exit_body_copy(worker_state), content_width).len();
        let action_height = textwrap::wrap(exit_action_copy(phase), content_width).len();
        u16::try_from(
            3usize
                .saturating_add(body_height)
                .saturating_add(action_height),
        )
        .unwrap_or(u16::MAX)
    }

    pub(super) fn render_exit_confirm_overlay(
        &self,
        frame: &mut Frame<'_>,
        screen_size: ratatui::layout::Rect,
        modal: &ModalState,
    ) {
        let max_height = (screen_size.height / 2).max(5);
        let height = self
            .exit_confirm_modal_height(screen_size.width.saturating_sub(2), modal)
            .clamp(5, max_height);
        let area = ratatui::layout::Rect::new(
            screen_size.x,
            screen_size.y + screen_size.height.saturating_sub(height),
            screen_size.width,
            height,
        );
        self.render_exit_confirm_modal(frame, area, modal);
    }

    pub(super) fn render_exit_confirm_modal(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        modal: &ModalState,
    ) {
        let Some((worker_state, phase)) = confirmation_parts(modal) else {
            return;
        };
        let content_width = area.width.saturating_sub(2).max(1) as usize;
        let body = textwrap::wrap(exit_body_copy(worker_state), content_width);
        let actions = textwrap::wrap(exit_action_copy(phase), content_width);
        let mut lines = body
            .into_iter()
            .map(|line| Line::from(Span::styled(line.into_owned(), Style::default())))
            .collect::<Vec<_>>();
        lines.push(Line::default());
        lines.extend(actions.into_iter().map(|line| {
            Line::from(Span::styled(
                line.into_owned(),
                Style::default().fg(Color::DarkGray),
            ))
        }));

        frame.render_widget(ratatui::widgets::Clear, area);
        let tray = Paragraph::new(lines).block(
            Block::default()
                .title("Agent is still working")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(tray, area);
    }
}

impl Tui {
    pub(super) fn handle_confirm_exit_key(&mut self, phase: ExitPhase, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.app.modal = None;
                self.abort_signal.set_ctrld();
                self.app.should_quit = true;
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                self.exit_after_cancel = false;
                self.app.modal = None;
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('r' | 'R'), KeyModifiers::NONE | KeyModifiers::SHIFT)
                if phase != ExitPhase::Interrupting =>
            {
                if phase == ExitPhase::Prompting
                    && !self.app.llm_busy
                    && self.cancellation.is_none()
                {
                    self.app.modal = None;
                    self.app.should_quit = true;
                    return;
                }
                if let Some(ModalState::ConfirmExit { phase, .. }) = self.app.modal.as_mut() {
                    *phase = ExitPhase::Interrupting;
                }
                if !self.start_exit_cancel() {
                    self.app.modal = None;
                    self.app.should_quit = true;
                }
            }
            _ => {}
        }
    }
}
