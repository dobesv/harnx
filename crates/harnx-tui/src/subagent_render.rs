//! Fullscreen rendering for independently monitored nested sessions.

use crate::markdown_render::RenderedEntry;
use crate::types::{
    MonitoredSessionKey, MonitoredSessionState, RenderEntryState, SubAgentInvocationProgress,
    SubAgentStatus, TranscriptItem, Tui, SPINNER_FRAMES,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

const TITLE_SEPARATOR: &str = "  ";
const MIN_TITLE_WIDTH: usize = 4;

pub(super) fn render_subagent_row(
    item: &TranscriptItem,
    spinner_index: usize,
    width: u16,
) -> RenderedEntry {
    let TranscriptItem::SubAgentSession {
        key,
        status,
        progress,
        ..
    } = item
    else {
        unreachable!("sub-agent row renderer requires a sub-agent item");
    };
    let short_id: String = key.session_id.chars().take(8).collect();
    let (icon, color) = status_icon(status, spinner_index);
    let metrics = progress.as_ref().map(format_progress).unwrap_or_default();
    let identity = format!("{icon} @ {}", key.agent);
    let status_metrics = format!(" [{short_id}]  {}{metrics}", status.label());
    let prefix_width = UnicodeWidthStr::width(identity.as_str())
        .saturating_add(UnicodeWidthStr::width(status_metrics.as_str()));
    let title = title_suffix(
        progress
            .as_ref()
            .and_then(|progress| progress.snapshot.title.as_deref()),
        usize::from(width),
        prefix_width,
    );
    RenderedEntry::from_lines(
        vec![Line::from(vec![
            Span::styled(identity, Style::default().fg(color)),
            Span::styled(
                format!("{status_metrics}{title}"),
                Style::default().fg(Color::DarkGray),
            ),
        ])],
        width,
    )
}

pub(super) fn render_subagent_detail(item: &TranscriptItem) -> Vec<Line<'static>> {
    let TranscriptItem::SubAgentSession {
        key,
        status,
        progress,
        ..
    } = item
    else {
        unreachable!("sub-agent detail renderer requires a sub-agent item");
    };
    let label = Style::default().fg(Color::DarkGray);
    let field = |name: &str, value: &str| {
        Line::from(vec![
            Span::styled(format!("{name}: "), label),
            Span::raw(value.to_string()),
        ])
    };
    let mut lines = vec![
        Line::from(Span::styled("── sub-agent ──", label)),
        field("agent", &key.agent),
        field("session_id", &key.session_id),
        field("cluster", &key.cluster),
        field("status", status.label()),
    ];
    if let Some(progress) = progress.as_ref() {
        if let Some(title) = progress.snapshot.title.as_deref().map(sanitize_title) {
            lines.push(field("title", &title));
        }
        lines.extend([
            field("invocation_id", &progress.snapshot.invocation_id),
            field("elapsed", &format_elapsed(progress.elapsed_ms())),
            field(
                "input_tokens",
                &progress.snapshot.usage.input_tokens.to_string(),
            ),
            field(
                "output_tokens",
                &progress.snapshot.usage.output_tokens.to_string(),
            ),
            field(
                "cached_tokens",
                &progress.snapshot.usage.cached_tokens.to_string(),
            ),
            field("tool_calls", &progress.snapshot.tool_call_count.to_string()),
        ]);
    }
    lines
}

pub(crate) fn title_suffix(title: Option<&str>, line_width: usize, prefix_width: usize) -> String {
    let Some(title) = title.map(sanitize_title).filter(|title| !title.is_empty()) else {
        return String::new();
    };
    let Some(title_budget) = line_width
        .checked_sub(prefix_width)
        .and_then(|remaining| remaining.checked_sub(UnicodeWidthStr::width(TITLE_SEPARATOR)))
        .filter(|budget| *budget >= MIN_TITLE_WIDTH)
    else {
        return String::new();
    };

    if UnicodeWidthStr::width(title.as_str()) <= title_budget {
        return format!("{TITLE_SEPARATOR}{title}");
    }

    let content_budget = title_budget.saturating_sub(UnicodeWidthStr::width("…"));
    let mut end = 0;
    for (index, character) in title.char_indices() {
        let candidate_end = index + character.len_utf8();
        if UnicodeWidthStr::width(&title[..candidate_end]) > content_budget {
            break;
        }
        end = candidate_end;
    }
    format!("{TITLE_SEPARATOR}{}…", &title[..end])
}

pub(crate) fn sanitize_title(title: &str) -> String {
    let mut sanitized = String::with_capacity(title.len());
    let mut replacing_control = false;
    for character in title.chars() {
        if character.is_control() {
            if !replacing_control {
                sanitized.push(' ');
                replacing_control = true;
            }
        } else {
            sanitized.push(character);
            replacing_control = false;
        }
    }
    sanitized
}

fn format_progress(progress: &SubAgentInvocationProgress) -> String {
    format!(
        "  {}  ↘ {}  ↗ {}  ◌ {}  ◔ {}",
        format_elapsed(progress.elapsed_ms()),
        progress.snapshot.usage.input_tokens,
        progress.snapshot.usage.output_tokens,
        progress.snapshot.usage.cached_tokens,
        progress.snapshot.tool_call_count,
    )
}

fn format_elapsed(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1_000;
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::{
        format_elapsed, render_child_header, render_subagent_detail, render_subagent_row,
        ChildHeader,
    };
    use crate::markdown_render::MarkdownBlockData;
    use crate::types::{
        MonitoredSessionKey, SubAgentInvocationProgress, SubAgentStatus, TranscriptItem,
    };
    use harnx_core::api_types::CompletionTokenUsage;
    use harnx_core::event::{SubAgentProgress, SubAgentProgressStatus};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    use unicode_width::UnicodeWidthStr;

    fn progress(title: Option<&str>) -> SubAgentInvocationProgress {
        SubAgentInvocationProgress::new(SubAgentProgress {
            invocation_id: "invocation".into(),
            agent: "worker".into(),
            session_id: "12345678abcdef".into(),
            status: SubAgentProgressStatus::Done,
            elapsed_ms: 1_000,
            usage: CompletionTokenUsage::new(Some(12), Some(3), Some(2)),
            tool_call_count: 1,
            title: title.map(str::to_string),
        })
    }

    fn item(title: Option<&str>) -> TranscriptItem {
        TranscriptItem::SubAgentSession {
            key: MonitoredSessionKey {
                cluster: "local".into(),
                agent: "worker".into(),
                session_id: "12345678abcdef".into(),
            },
            status: SubAgentStatus::Completed,
            invocation_id: Some("invocation".into()),
            progress: Some(progress(title)),
        }
    }

    fn row_text(title: Option<&str>, width: u16) -> (String, u16) {
        let rendered = render_subagent_row(&item(title), 0, width);
        let MarkdownBlockData::Paragraph { lines, .. } = &rendered.blocks[0] else {
            panic!("expected paragraph");
        };
        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        (text, rendered.total_height)
    }

    fn header_line(title: Option<&str>, width: u16) -> String {
        let key = MonitoredSessionKey {
            cluster: "local".into(),
            agent: "worker".into(),
            session_id: "12345678abcdef".into(),
        };
        let progress = progress(title);
        let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
        terminal
            .draw(|frame| {
                render_child_header(
                    frame,
                    Rect::new(0, 0, width, 2),
                    ChildHeader {
                        key: &key,
                        status: &SubAgentStatus::Completed,
                        progress: Some(&progress),
                        spinner_index: 0,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut line = String::new();
        for x in 0..width {
            line.push_str(buffer[(x, 0)].symbol());
        }
        line.trim_end().to_string()
    }

    #[test]
    fn elapsed_time_uses_completed_whole_seconds() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(1_999), "1s");
        assert_eq!(format_elapsed(12_345), "12s");
    }

    #[test]
    fn long_row_title_truncates_by_display_width_without_wrapping() {
        let (prefix, _) = row_text(None, 200);
        let width = UnicodeWidthStr::width(prefix.as_str()) + 2 + 6;
        let (text, height) = row_text(Some("ab😀cdef"), width as u16);

        assert_eq!(text, format!("{prefix}  ab😀c…"));
        assert_eq!(UnicodeWidthStr::width(text.as_str()), width);
        assert_eq!(height, 1);
    }

    #[test]
    fn short_row_title_is_shown_in_full() {
        let (prefix, _) = row_text(None, 200);
        let title = "checking docs";
        let width = UnicodeWidthStr::width(prefix.as_str()) + 2 + UnicodeWidthStr::width(title);
        let (text, height) = row_text(Some(title), width as u16);

        assert_eq!(text, format!("{prefix}  {title}"));
        assert_eq!(height, 1);
    }

    #[test]
    fn absent_row_title_preserves_previous_output() {
        let (text, height) = row_text(None, 200);

        assert_eq!(text, "✓ @ worker [12345678]  done  1s  ↘ 12  ↗ 3  ◌ 2  ◔ 1");
        assert_eq!(height, 1);
    }

    #[test]
    fn narrow_row_drops_title() {
        let (prefix, _) = row_text(None, 200);
        let width = UnicodeWidthStr::width(prefix.as_str()) + 2 + 3;
        let (text, height) = row_text(Some("visible only with enough room"), width as u16);

        assert_eq!(text, prefix);
        assert_eq!(height, 1);
    }

    #[test]
    fn child_header_uses_same_title_truncation() {
        let prefix = header_line(None, 200);
        let width = UnicodeWidthStr::width(prefix.as_str()) + 2 + 8;
        let line = header_line(Some("abcdefghijk"), width as u16);

        assert_eq!(line, format!("{prefix}  abcdefg…"));
        assert_eq!(UnicodeWidthStr::width(line.as_str()), width);
    }

    #[test]
    fn detail_title_collapses_control_characters() {
        let lines = render_subagent_detail(&item(Some("line\r\nbreak\u{7f}done")));
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert!(text.iter().any(|line| line == "title: line break done"));
    }
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
        let Some(view) = self.app.subagent_view_stack.last().cloned() else {
            return;
        };
        let key = view.key;
        let header_height = if view.progress.is_some() { 2 } else { 1 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_height),
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
                status: &view.status,
                progress: view.progress.as_ref(),
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
        let can_stop = matches!(
            view.status,
            SubAgentStatus::Running | SubAgentStatus::Unconfirmed
        ) && view
            .progress
            .as_ref()
            .is_some_and(|p| state.execution_id.as_deref() == Some(&p.snapshot.invocation_id));
        render_child_footer(frame, chunks[2], can_stop);
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

fn render_child_footer(frame: &mut Frame<'_>, area: Rect, can_stop: bool) {
    let stop = if can_stop { "  Ctrl+C/Stop" } else { "" };
    frame.render_widget(
        Paragraph::new(format!(
            " ↑↓/browse  ENTER/open  PgUp/PgDn/scroll  g/G top/bot  ESC/back{stop}"
        ))
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
    progress: Option<&'a SubAgentInvocationProgress>,
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
    let identity = format!(" {icon} @ {}", header.key.agent);
    let status = format!(" [{short_id}]  {}", header.status.label());
    let prefix_width = UnicodeWidthStr::width(identity.as_str())
        .saturating_add(UnicodeWidthStr::width(status.as_str()));
    let title = title_suffix(
        header
            .progress
            .and_then(|progress| progress.snapshot.title.as_deref()),
        usize::from(area.width),
        prefix_width,
    );
    let mut lines = vec![Line::from(vec![
        Span::styled(identity, Style::default().fg(color)),
        Span::styled(
            format!("{status}{title}"),
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    if let Some(progress) = header.progress {
        lines.push(Line::from(Span::styled(
            format!(" {}", format_progress(progress).trim()),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
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
                RenderEntryState::new(Some(index) == streaming_idx, render.options.spinner_index),
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
        SubAgentStatus::Cancelling => ("…", Color::Yellow),
        SubAgentStatus::Cancelled => ("■", Color::DarkGray),
        SubAgentStatus::Unconfirmed => ("!", Color::Yellow),
    }
}
