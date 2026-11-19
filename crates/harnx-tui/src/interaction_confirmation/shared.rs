//! Shared pieces used by both the T4.2 interaction modals and the T5.1
//! non-modal surface form (plan T5.1 Part B), so the per-concern modules stay
//! free of duplicated layout and attach logic.

use super::{a2ui, a2ui_render};

/// Crate-visible form renderer for non-modal surfaces (plan T5.1): reuses the
/// T4.2 A2UI widgets instead of growing a second form renderer.
pub(crate) fn render_a2ui_form(
    form: &a2ui::A2UIFormState,
    width: u16,
) -> Vec<ratatui::text::Line<'static>> {
    a2ui_render::render_form_lines(form, width)
}

/// Shared attach for the per-concern session observers (T4.2 interactions and
/// T5.1 surfaces): both watch the same durable log stream.
pub(crate) async fn attach_event_stream(
    config: &harnx_runtime::config::GlobalConfig,
    target: &(String, String),
) -> anyhow::Result<harnx_runtime::nats_event_sink::SessionEventStream> {
    let snapshot = config.read().clone();
    let client = snapshot.nats_client(&target.1).await?;
    harnx_runtime::nats_event_sink::SessionEventStream::attach(
        async_nats::jetstream::new(client.clone()),
        client,
        &target.0,
    )
    .await
}

/// Shared note-area rendering for the A2UI modals (T4.2 interaction
/// confirmation and the T5.1 surface action form).
pub(crate) fn render_note_area(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    note: &mut ratatui_textarea::TextArea<'static>,
    focused: bool,
    title: &'static str,
) {
    let border_style = if focused {
        ratatui::style::Style::default().fg(ratatui::style::Color::Yellow)
    } else {
        ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray)
    };
    note.set_block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::TOP)
            .title(title)
            .border_style(border_style),
    );
    note.set_cursor_line_style(ratatui::style::Style::default());
    note.set_wrap_mode(ratatui_textarea::WrapMode::Word);
    frame.render_widget(&*note, area);
}

/// Shared A2UI modal body layout (T4.2 interactions, T5.1 surfaces): split
/// the modal's inner area into body/note/footer, clamp the scroll to the body
/// height, render body and footer, and return the note area.
/// The modal pieces `render_modal_body` lays out: body and footer content,
/// plus the note area's height.
pub(crate) struct ModalBodyParts {
    pub body: Vec<ratatui::text::Line<'static>>,
    pub footer: ratatui::text::Text<'static>,
    pub note_height: u16,
}

pub(crate) fn render_modal_body(
    frame: &mut ratatui::Frame<'_>,
    inner: ratatui::layout::Rect,
    parts: ModalBodyParts,
    scroll: &mut u16,
) -> ratatui::layout::Rect {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::style::{Color, Style};
    use ratatui::widgets::{Paragraph, Wrap};

    let footer_height = Paragraph::new(parts.footer.clone())
        .wrap(Wrap { trim: false })
        .line_count(inner.width.max(1));
    let regions = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(parts.note_height),
        Constraint::Length(u16::try_from(footer_height).unwrap_or(u16::MAX)),
    ])
    .split(inner);
    let paragraph = Paragraph::new(parts.body).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(regions[0].width.max(1))
        .saturating_sub(usize::from(regions[0].height));
    *scroll = (*scroll).min(u16::try_from(max_scroll).unwrap_or(u16::MAX));
    frame.render_widget(paragraph.scroll((*scroll, 0)), regions[0]);
    frame.render_widget(
        Paragraph::new(parts.footer)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Yellow)),
        regions[2],
    );
    regions[1]
}
