//! Rendering of the interaction tray: modal geometry and body lines.

use super::{footer::interaction_footer, InteractionModal, Tui};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

const NOTE_HEIGHT: u16 = 3;
const BORDERS_HEIGHT: usize = 2;
pub(crate) const MIN_MODAL_HEIGHT: u16 = 6;
const A2UI_BODY_COLUMNS: u16 = 80;

// qual:allow(coupling, oi) reason: "Rendering and key handling extend the shared TUI, following the existing per-concern modules."
impl Tui {
    pub(crate) fn interaction_modal_height(&self, width: u16, state: &InteractionModal) -> u16 {
        let body = interaction_body(state);
        let height = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .line_count(width.saturating_sub(2).max(1));
        let footer = interaction_footer(
            state,
            self.interactions.read_error.as_deref(),
            self.app.footer_notice_text(),
        );
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
            } else if state.a2ui.is_some() || state.pending.kind == "url" {
                "Interaction"
            } else {
                "Middleware confirmation"
            })
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // `url` interactions carry no note (decision 6: the stored response is
        // the action alone), so their modal shows no note area.
        let is_url = state.pending.kind == "url";
        let note_area = super::render_modal_body(
            frame,
            inner,
            super::ModalBodyParts {
                body: interaction_body(state),
                footer: interaction_footer(
                    state,
                    self.interactions.read_error.as_deref(),
                    self.app.footer_notice_text(),
                )
                .into(),
                note_height: if is_url { 0 } else { NOTE_HEIGHT },
            },
            &mut state.scroll,
        );
        let note_focused = state
            .a2ui
            .as_ref()
            .is_some_and(|f| f.focus == super::a2ui::FocusTarget::Note);
        if !is_url {
            super::render_note_area(
                frame,
                note_area,
                &mut state.note,
                note_focused,
                "Note (optional)",
            );
        }
    }
}

fn interaction_body(state: &InteractionModal) -> Vec<Line<'static>> {
    if let Some(form) = &state.a2ui {
        let mut lines = super::a2ui_render::render_form_lines(form, A2UI_BODY_COLUMNS);
        lines.extend(more_pending_lines(state));
        return lines;
    }
    if state.pending.kind == "url" {
        return url_body_lines(state);
    }
    confirm_body_lines(state)
}

/// `2 more pending; shown one at a time.` — only one request is ever on
/// screen; this says how many queue behind it.
fn more_pending_lines(state: &InteractionModal) -> Vec<Line<'static>> {
    if state.pending.pending_count == 0 {
        return Vec::new();
    }
    vec![
        Line::from(""),
        Line::from(format!(
            "{} more pending; shown one at a time.",
            state.pending.pending_count
        )),
    ]
}

fn url_body_lines(state: &InteractionModal) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(state.pending.message.clone()), Line::from("")];
    let url = state
        .pending
        .payload
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    // Plain text on screen; the OSC 8 wrapping is added by the terminal
    // writer as the frame leaves (decision 6).
    lines.push(crate::hyperlink::url_span(url));
    lines.extend(more_pending_lines(state));
    lines
}

fn confirm_body_lines(state: &InteractionModal) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(state.pending.message.clone())];
    if state.pending.kind != "confirm" {
        lines.push(Line::from(format!(
            "Interaction kind '{}' is not supported by this client. Deny only.",
            state.pending.kind
        )));
    }
    lines.extend(more_pending_lines(state));
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
