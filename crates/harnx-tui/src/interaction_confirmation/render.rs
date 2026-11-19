use super::{InteractionModal, Tui};
use crate::tool_confirmation::TOOL_CONFIRM_IDLE_GATE;
use crate::types::ModalState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::time::Instant;

const SCROLL_PAGE_LINES: u16 = 10;
const NOTE_HEIGHT: u16 = 3;
const BORDERS_HEIGHT: usize = 2;
pub(crate) const MIN_MODAL_HEIGHT: u16 = 6;

// qual:allow(coupling, oi) reason: "Rendering and key handling extend the shared TUI, following the existing per-concern modules."
impl Tui {
    pub(crate) fn paste_interaction_note(&mut self, text: &str) -> bool {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return false;
        };
        if !state.submitting {
            state.last_key_at = Instant::now();
            state.note.insert_str(text);
        }
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
            } else {
                "Middleware confirmation"
            })
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let footer = interaction_footer(state, self.interactions.read_error.as_deref());
        let footer_height = Paragraph::new(footer.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner.width.max(1));
        let regions = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(NOTE_HEIGHT),
            Constraint::Length(u16::try_from(footer_height).unwrap_or(u16::MAX)),
        ])
        .split(inner);
        let body = Paragraph::new(interaction_body(state)).wrap(Wrap { trim: false });
        let max_scroll = body
            .line_count(regions[0].width.max(1))
            .saturating_sub(usize::from(regions[0].height));
        state.scroll = state
            .scroll
            .min(u16::try_from(max_scroll).unwrap_or(u16::MAX));
        frame.render_widget(body.scroll((state.scroll, 0)), regions[0]);
        state.note.set_block(
            Block::default()
                .borders(Borders::TOP)
                .title("Note (optional)"),
        );
        state.note.set_cursor_line_style(Style::default());
        state.note.set_wrap_mode(ratatui_textarea::WrapMode::Word);
        frame.render_widget(&state.note, regions[1]);
        frame.render_widget(
            Paragraph::new(footer)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::Yellow)),
            regions[2],
        );
    }
}

fn interaction_body(state: &InteractionModal) -> Vec<Line<'static>> {
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
