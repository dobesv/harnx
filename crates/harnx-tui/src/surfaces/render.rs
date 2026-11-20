//! Surface action form rendering and key handling. The form reuses the T4.2
//! A2UI widgets; the differences from the modal-interaction form: no deny
//! shortcut (`deny` is not a surface action), and Esc closes without
//! submitting — surfaces are non-modal, so closing cancels nothing.
use super::{SurfaceActionModal, Tui};
use crate::markdown_render::RenderedEntry;
use crate::types::{ModalState, TranscriptItem};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::time::Instant;

const SCROLL_PAGE_LINES: u16 = 10;
const NOTE_HEIGHT: u16 = 3;
const BORDERS_HEIGHT: usize = 2;
const BODY_COLUMNS: u16 = 80;

// qual:allow(coupling, oi) reason: "Rendering and key handling extend the shared Tui, following the per-concern module split."
impl Tui {
    /// Transcript row for a published surface. Outstanding rows invite the
    /// user to act; resolved ones show the accepted action and stop.
    // qual:allow(complexity, error_handling) reason: "the unreachable arm documents a render-only invariant, like the session-row renderer's"
    pub(crate) fn render_surface_row(item: &TranscriptItem, width: u16) -> RenderedEntry {
        let TranscriptItem::UserSurface {
            surface_id: _,
            payload,
            resolved,
        } = item
        else {
            unreachable!("render_surface_row only accepts surface items")
        };
        let headline = super::surface_headline(payload);
        let status = match resolved {
            Some(action) => format!(" · answered: {action}"),
            None => " · Enter to act".to_string(),
        };
        let headline_color = if resolved.is_some() {
            Color::DarkGray
        } else {
            Color::Cyan
        };
        let line = Line::from(vec![
            Span::styled("▣ surface: ", Style::default().fg(headline_color)),
            Span::styled(headline, Style::default().fg(headline_color)),
            Span::styled(status, Style::default().fg(Color::DarkGray)),
        ]);
        RenderedEntry::from_lines(vec![line], width)
    }

    // qual:allow(complexity, max_cyclomatic=12) reason: "key dispatch mirrors handle_interaction_key's flat match, per key kind"
    pub(crate) fn handle_surface_key(&mut self, key: KeyEvent) {
        let Some(ModalState::SurfaceAction(state)) = self.app.modal.as_mut() else {
            return;
        };
        if state.submitting {
            return;
        }
        let now = Instant::now();
        match (key.code, key.modifiers) {
            // Non-modal: closing the form cancels nothing, so Esc suffices.
            (KeyCode::Esc, KeyModifiers::NONE) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.app.modal = None;
                return;
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                state.last_key_at = now;
                if let Some(form) = state.a2ui.as_mut() {
                    form.focus_next();
                }
                return;
            }
            (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => {
                state.last_key_at = now;
                if let Some(form) = state.a2ui.as_mut() {
                    form.focus_prev();
                }
                return;
            }
            (KeyCode::PageUp, _) => {
                state.scroll = state.scroll.saturating_sub(SCROLL_PAGE_LINES);
                return;
            }
            (KeyCode::PageDown, _) => {
                state.scroll = state.scroll.saturating_add(SCROLL_PAGE_LINES);
                return;
            }
            _ => {}
        }
        let note_focused = state.a2ui.as_ref().is_some_and(|form| {
            form.focus == crate::interaction_confirmation::a2ui::FocusTarget::Note
        });
        if note_focused {
            self.handle_surface_note_key(key, now);
            return;
        }
        state.last_key_at = now;
        if let Some(form) = state.a2ui.as_mut() {
            if let Some(crate::interaction_confirmation::a2ui::A2UIAction::Submit(name)) =
                form.handle_key(key)
            {
                self.submit_surface_action(name);
            }
        }
    }

    fn handle_surface_note_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(ModalState::SurfaceAction(state)) = self.app.modal.as_mut() else {
            return;
        };
        let primary = state.a2ui.as_ref().and_then(|form| form.primary_action());
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                state.last_key_at = now;
                state.note.insert_newline();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if let Some(action) = primary {
                    self.submit_surface_action(action);
                }
            }
            _ => {
                state.last_key_at = now;
                state.note.input(key);
            }
        }
    }

    pub(crate) fn paste_surface_note(&mut self, text: &str) -> bool {
        let Some(ModalState::SurfaceAction(state)) = self.app.modal.as_mut() else {
            return false;
        };
        if state.submitting {
            return true;
        }
        state.last_key_at = Instant::now();
        let pasted_into_form = state
            .a2ui
            .as_mut()
            .is_some_and(|form| form.paste_text(text));
        if !pasted_into_form {
            state.note.insert_str(text);
        }
        true
    }
}

pub(crate) fn surface_modal_height(width: u16, state: &SurfaceActionModal) -> u16 {
    let body = surface_body(state);
    let height = Paragraph::new(body)
        .wrap(Wrap { trim: false })
        .line_count(width.saturating_sub(2).max(1));
    let footer = surface_footer(state);
    let footer_height = Paragraph::new(footer)
        .wrap(Wrap { trim: false })
        .line_count(width.saturating_sub(2).max(1));
    u16::try_from(
        height
            .saturating_add(footer_height)
            .saturating_add(usize::from(NOTE_HEIGHT) + BORDERS_HEIGHT),
    )
    .unwrap_or(u16::MAX)
}

pub(crate) fn render_surface_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut SurfaceActionModal,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Surface action")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let note_area = crate::interaction_confirmation::render_modal_body(
        frame,
        inner,
        crate::interaction_confirmation::ModalBodyParts {
            body: surface_body(state),
            footer: surface_footer(state).into(),
            note_height: NOTE_HEIGHT,
        },
        &mut state.scroll,
    );
    let note_focused = state
        .a2ui
        .as_ref()
        .is_some_and(|form| form.focus == crate::interaction_confirmation::a2ui::FocusTarget::Note);
    crate::interaction_confirmation::render_note_area(
        frame,
        note_area,
        &mut state.note,
        note_focused,
        "Message (optional)",
    );
}

fn surface_body(state: &SurfaceActionModal) -> Vec<Line<'static>> {
    match &state.a2ui {
        Some(form) => crate::interaction_confirmation::render_a2ui_form(form, BODY_COLUMNS),
        None => vec![
            Line::from(super::surface_headline(&state.snapshot.payload)),
            Line::from(""),
            Line::from("This surface has no actionable buttons."),
        ],
    }
}

fn surface_footer(state: &SurfaceActionModal) -> String {
    let help = if state.submitting {
        "Submitting… · Esc close".to_string()
    } else if state.a2ui.is_some() {
        "Enter activate · Tab navigate · Space select · Shift+Enter newline · Esc close · PgUp/PgDn scroll"
            .to_string()
    } else {
        "Esc close".to_string()
    };
    match state.error.as_deref() {
        Some(error) => format!("{help}\n{error}"),
        None => help,
    }
}
