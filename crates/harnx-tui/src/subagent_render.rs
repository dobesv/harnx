//! Fullscreen rendering for independently monitored nested sessions.

use crate::markdown_render::RenderedEntry;
use crate::types::{
    MonitoredSessionKey, MonitoredSessionState, SubAgentStatus, TranscriptItem, Tui, SPINNER_FRAMES,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub(super) fn render_subagent_row(
    key: &MonitoredSessionKey,
    status: &SubAgentStatus,
    width: u16,
) -> RenderedEntry {
    let short_id: String = key.session_id.chars().take(8).collect();
    let (icon, color) = match status {
        SubAgentStatus::Running => ("●", Color::Yellow),
        SubAgentStatus::Completed => ("✓", Color::Green),
        SubAgentStatus::Failed => ("✗", Color::Red),
    };
    RenderedEntry::from_lines(
        vec![Line::from(vec![
            Span::styled(
                format!("{icon} @ {}", key.agent),
                Style::default().fg(color),
            ),
            Span::styled(
                format!(" [{short_id}]  {}", status.label()),
                Style::default().fg(Color::DarkGray),
            ),
        ])],
        width,
    )
}

pub(super) fn render_subagent_detail(
    key: &MonitoredSessionKey,
    status: &SubAgentStatus,
) -> Vec<Line<'static>> {
    let label = Style::default().fg(Color::DarkGray);
    let field = |name: &str, value: &str| {
        Line::from(vec![
            Span::styled(format!("{name}: "), label),
            Span::raw(value.to_string()),
        ])
    };
    vec![
        Line::from(Span::styled("── sub-agent ──", label)),
        field("agent", &key.agent),
        field("session_id", &key.session_id),
        field("cluster", &key.cluster),
        field("status", status.label()),
    ]
}

impl Tui {
    pub(super) fn render_exclusive_transcript(
        &mut self,
        frame: &mut Frame<'_>,
        size: Rect,
    ) -> bool {
        if self.app.detail_view_open {
            self.render_detail_view(frame, size);
        } else if !self.app.subagent_view_stack.is_empty() {
            self.render_subagent_session_view(frame, size);
        } else if self.app.transcript_browsing {
            self.render_browsing_view(frame, size);
        } else {
            return false;
        }
        true
    }

    fn render_subagent_session_view(&mut self, frame: &mut Frame<'_>, size: Rect) {
        frame.render_widget(ratatui::widgets::Clear, size);
        let Some(key) = self.app.subagent_view_stack.last().cloned() else {
            return;
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(size);
        let options = ChildRenderOptions {
            show_seq: self.app.show_sequence_numbers,
            show_ts: self.app.show_timestamps,
            use_utc: self.app.use_utc_timestamps,
            spinner_index: self.app.spinner_index,
        };
        let theme = self.code_theme.as_ref();
        let Some(state) = self.app.monitored_sessions.get_mut(&key) else {
            frame.render_widget(
                Paragraph::new("Waiting for sub-agent session history…")
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
            return;
        };

        render_child_header(
            frame,
            chunks[0],
            ChildHeader {
                key: &key,
                status: &state.status,
                spinner_index: options.spinner_index,
            },
        );
        let entries = render_child_entries(
            &mut state.transcript,
            ChildEntryRender {
                focus: state.transcript_focus,
                streaming_open: state.streaming_open,
                width: chunks[1].width,
                options,
                theme,
            },
        );
        scroll_focused_child_into_view(state, chunks[1]);
        state.scroll.render(frame, chunks[1], &entries, |entry| {
            (entry.total_height as usize, entry.clone())
        });
        if !state.scroll.follow {
            state.scroll.position = state.scroll.position.min(state.scroll.last_max_position);
        }
        render_child_footer(frame, chunks[2]);
    }
}

fn scroll_focused_child_into_view(state: &mut MonitoredSessionState, area: Rect) {
    if !state.scroll_to_focused_item {
        return;
    }
    if let Some(focus) = state.transcript_focus {
        state.scroll.position = state.scroll.scroll_position_to_show_item(
            focus,
            area.width,
            area.height as usize,
            state.transcript.len(),
        );
    }
    state.scroll_to_focused_item = false;
}

fn render_child_footer(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new(" ↑↓/browse  ENTER/open  PgUp/PgDn/scroll  ESC/back")
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

#[derive(Clone, Copy)]
struct ChildRenderOptions {
    show_seq: bool,
    show_ts: bool,
    use_utc: bool,
    spinner_index: usize,
}

struct ChildHeader<'a> {
    key: &'a MonitoredSessionKey,
    status: &'a SubAgentStatus,
    spinner_index: usize,
}

struct ChildEntryRender<'a> {
    focus: Option<usize>,
    streaming_open: bool,
    width: u16,
    options: ChildRenderOptions,
    theme: Option<&'a syntect::highlighting::Theme>,
}

fn render_child_header(frame: &mut Frame<'_>, area: Rect, header: ChildHeader<'_>) {
    let (icon, color) = status_icon(header.status, header.spinner_index);
    let short_id: String = header.key.session_id.chars().take(8).collect();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {icon} @ {}", header.key.agent),
                Style::default().fg(color),
            ),
            Span::styled(
                format!(" [{short_id}]  {}", header.status.label()),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        area,
    );
}

fn render_child_entries(
    transcript: &mut [TranscriptItem],
    render: ChildEntryRender<'_>,
) -> Vec<RenderedEntry> {
    let streaming_idx = render
        .streaming_open
        .then(|| transcript.len().checked_sub(1))
        .flatten();
    let entries = transcript
        .iter_mut()
        .enumerate()
        .map(|(index, item)| {
            let mut rendered = Tui::render_entry(
                item,
                render.options.show_seq,
                render.options.show_ts,
                render.options.use_utc,
                render.width,
                Some(index) == streaming_idx,
                render.theme,
            );
            if render.focus == Some(index) {
                rendered.reverse_style();
            }
            rendered
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        vec![RenderedEntry::from_lines(
            vec![Line::from(Span::styled(
                "Waiting for transcript…",
                Style::default().fg(Color::DarkGray),
            ))],
            render.width,
        )]
    } else {
        entries
    }
}

fn status_icon(status: &SubAgentStatus, spinner_index: usize) -> (&'static str, Color) {
    match status {
        SubAgentStatus::Running => (
            SPINNER_FRAMES[spinner_index % SPINNER_FRAMES.len()],
            Color::Yellow,
        ),
        SubAgentStatus::Completed => ("✓", Color::Green),
        SubAgentStatus::Failed => ("✗", Color::Red),
    }
}
