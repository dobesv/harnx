use super::{InteractionModal, Tui};
use crate::tool_confirmation::TOOL_CONFIRM_IDLE_GATE;
use crate::types::ModalState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::time::Instant;

const SCROLL_PAGE_LINES: u16 = 10;
const NOTE_HEIGHT: u16 = 3;
const BORDERS_HEIGHT: usize = 2;
pub(crate) const MIN_MODAL_HEIGHT: u16 = 6;
const A2UI_BODY_COLUMNS: u16 = 80;

// qual:allow(coupling, oi) reason: "Rendering and key handling extend the shared TUI, following the existing per-concern modules."
impl Tui {
    pub(crate) fn paste_interaction_note(&mut self, text: &str) -> bool {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return false;
        };
        insert_pasted_text(state, text);
        true
    }

    pub(crate) async fn handle_interaction_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            if self.interrupt_child_confirmation() {
                return;
            }
            self.clear_interaction_modal();
            // The durable observer can beat the activity monitor on attach.
            self.active_remote_session = self.interactions.target.clone();
            self.handle_ctrl_c().await;
            return;
        }
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        if state.submitting {
            return;
        }
        let now = Instant::now();
        if state.a2ui.is_some() {
            self.handle_a2ui_interaction_key(key, now);
        } else {
            self.handle_confirm_interaction_key(key, now);
        }
    }

    fn handle_a2ui_interaction_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        let Some(form) = &mut state.a2ui else {
            return;
        };
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.submit_a2ui_interaction("deny".into());
                return;
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                state.last_key_at = now;
                form.focus_next();
                return;
            }
            (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => {
                state.last_key_at = now;
                form.focus_prev();
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

        if form.focus == super::a2ui::FocusTarget::Note {
            self.handle_a2ui_note_key(key, now);
            return;
        }

        state.last_key_at = now;
        if let Some(super::a2ui::A2UIAction::Submit(name)) = form.handle_key(key) {
            self.submit_a2ui_interaction(name);
        }
    }

    fn handle_a2ui_note_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        let primary = state.a2ui.as_ref().and_then(|f| f.primary_action());
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                state.last_key_at = now;
                state.note.insert_newline();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if let Some(action) = primary {
                    self.submit_a2ui_interaction(action);
                }
            }
            _ => {
                state.last_key_at = now;
                state.note.input(key);
            }
        }
    }

    fn handle_confirm_interaction_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if now.saturating_duration_since(state.last_key_at) >= TOOL_CONFIRM_IDLE_GATE {
                    self.submit_interaction(true);
                }
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => self.submit_interaction(false),
            (KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                state.last_key_at = now;
                state.note.insert_newline();
            }
            (KeyCode::PageUp, _) => state.scroll = state.scroll.saturating_sub(SCROLL_PAGE_LINES),
            (KeyCode::PageDown, _) => state.scroll = state.scroll.saturating_add(SCROLL_PAGE_LINES),
            _ => {
                state.last_key_at = now;
                state.note.input(key);
            }
        }
    }

    pub(crate) fn interaction_modal_height(&self, width: u16, state: &InteractionModal) -> u16 {
        let body = interaction_body(state);
        let height = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .line_count(width.saturating_sub(2).max(1));
        let footer = interaction_footer(state, self.interactions.read_error.as_deref());
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

    pub(crate) fn render_interaction_modal(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        state: &mut InteractionModal,
    ) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(if state.child.is_some() {
                "Child confirmation"
            } else if state.a2ui.is_some() {
                "Interaction"
            } else {
                "Middleware confirmation"
            })
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let note_area = super::render_modal_body(
            frame,
            inner,
            super::ModalBodyParts {
                body: interaction_body(state),
                footer: interaction_footer(state, self.interactions.read_error.as_deref()).into(),
                note_height: NOTE_HEIGHT,
            },
            &mut state.scroll,
        );
        let note_focused = state
            .a2ui
            .as_ref()
            .is_some_and(|f| f.focus == super::a2ui::FocusTarget::Note);
        super::render_note_area(
            frame,
            note_area,
            &mut state.note,
            note_focused,
            "Note (optional)",
        );
    }
}

fn insert_pasted_text(state: &mut InteractionModal, text: &str) {
    if state.submitting {
        return;
    }
    state.last_key_at = Instant::now();
    let pasted_into_form = state
        .a2ui
        .as_mut()
        .is_some_and(|form| form.paste_text(text));
    if !pasted_into_form {
        state.note.insert_str(text);
    }
}

fn interaction_body(state: &InteractionModal) -> Vec<Line<'static>> {
    if let Some(form) = &state.a2ui {
        let mut lines = super::a2ui_render::render_form_lines(form, A2UI_BODY_COLUMNS);
        if state.pending.pending_count > 0 {
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "{} more pending; shown one at a time.",
                state.pending.pending_count
            )));
        }
        return lines;
    }
    let mut lines = vec![Line::from(state.pending.message.clone())];
    if state.pending.kind != "confirm" {
        lines.push(Line::from(format!(
            "Interaction kind '{}' is not supported by this client. Deny only.",
            state.pending.kind
        )));
    }
    if state.pending.pending_count > 0 {
        lines.push(Line::from(format!(
            "{} more pending; shown one at a time.",
            state.pending.pending_count
        )));
    }
    // Literal text, not markdown: a confirm message cannot add links or actions.
    lines
        .into_iter()
        .flat_map(|line| {
            line.to_string()
                .split('\n')
                .map(|part| Line::from(part.to_string()))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn interaction_footer(state: &InteractionModal, read_error: Option<&str>) -> String {
    let read_error = read_error.filter(|_| state.child.is_none());
    let interrupt = if state.child.is_some() {
        "interrupt child"
    } else {
        "interrupt"
    };
    let help = if state.submitting {
        format!("Submitting… · Ctrl+C {interrupt}")
    } else if state.a2ui.is_some() {
        super::a2ui_render::footer_help(interrupt)
    } else if state.pending.kind != "confirm" {
        format!("Ctrl+D deny · Ctrl+C {interrupt} · PgUp/PgDn scroll")
    } else {
        let idle = Instant::now().saturating_duration_since(state.last_key_at);
        let approve = if idle < TOOL_CONFIRM_IDLE_GATE {
            format!(
                "ENTER approve ({:.1}s)",
                TOOL_CONFIRM_IDLE_GATE.saturating_sub(idle).as_secs_f32()
            )
        } else {
            "ENTER approve".to_string()
        };
        format!(
            "{approve} · Ctrl+D deny · Ctrl+C {interrupt} · Shift+Enter newline · PgUp/PgDn scroll"
        )
    };
    match read_error.or(state.error.as_deref()) {
        Some(error) => format!("{help}\n{error}"),
        None => help,
    }
}
