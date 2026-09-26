//! Rich in-place tool-call row rendering (Phase 5a, issue #2096).

use crate::markdown_render::{MarkdownBlockData, RenderedEntry};
use crate::subagent_render::{sanitize_title, title_suffix};
use crate::types::{ToolCallBody, TranscriptItem, Tui, SPINNER_FRAMES};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{ToolKind, ToolLocation, ToolStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::highlighting::Theme;
use unicode_width::UnicodeWidthStr;

/// Icon for each tool kind.
pub(crate) fn tool_kind_icon(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "⎘",
        ToolKind::Edit => "✎",
        ToolKind::Delete => "⌫",
        ToolKind::Move => "⇄",
        ToolKind::Search => "⌕",
        ToolKind::Execute => "⚡",
        ToolKind::Think => "◇",
        ToolKind::Fetch => "⤓",
        ToolKind::SwitchMode => "↻",
        ToolKind::Other => "→",
    }
}

/// Status icon and color for tool call row.
pub(crate) fn tool_status_icon(
    status: Option<ToolStatus>,
    is_running: bool,
    spinner_index: usize,
) -> (&'static str, Color) {
    match status {
        Some(ToolStatus::InProgress | ToolStatus::Pending) if is_running => (
            SPINNER_FRAMES[spinner_index % SPINNER_FRAMES.len()],
            Color::Yellow,
        ),
        Some(ToolStatus::Completed) => ("✓", Color::Green),
        Some(ToolStatus::Failed) => ("✗", Color::Red),
        _ => ("→", Color::DarkGray),
    }
}

/// Format a single tool location.
pub(crate) fn format_location(loc: &ToolLocation) -> String {
    let path = loc.path.display().to_string();
    match loc.line {
        Some(line) => format!("{path}:{line}"),
        None => path,
    }
}

/// Format multiple tool locations as a compact summary.
pub(crate) fn format_locations(locations: &[ToolLocation]) -> String {
    match locations.len() {
        0 => String::new(),
        1 => format_location(&locations[0]),
        2 => format!(
            "{}, {}",
            format_location(&locations[0]),
            format_location(&locations[1])
        ),
        n => format!(
            "{}, {} (+{} more)",
            format_location(&locations[0]),
            format_location(&locations[1]),
            n - 2
        ),
    }
}

pub(crate) fn tool_status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "pending",
        ToolStatus::InProgress => "in_progress",
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
    }
}

pub(crate) struct RenderToolCallArgs<'a> {
    pub tool_name: &'a str,
    pub body: Option<&'a ToolCallBody>,
    pub title: Option<&'a str>,
    pub status: Option<ToolStatus>,
    pub kind: Option<ToolKind>,
    pub locations: &'a [ToolLocation],
    pub is_running: bool,
    pub spinner_index: usize,
    pub timer: Option<&'a str>,
    pub width: u16,
    pub theme: Option<&'a Theme>,
}

pub(crate) fn render_tool_call(args: RenderToolCallArgs<'_>) -> RenderedEntry {
    let dim_gray = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);

    let has_live_updates = args.title.is_some()
        || matches!(args.kind, Some(k) if !matches!(k, ToolKind::Other))
        || !args.locations.is_empty();

    // Preserve existing markdown-body-suppresses-header behavior when
    // no live updates, not running, and no timer.
    if let Some(ToolCallBody::Markdown(md)) = args.body {
        if !has_live_updates && !args.is_running && args.timer.is_none() {
            return crate::markdown_render::render_markdown(md, dim_gray, args.width, args.theme);
        }
    }

    // Build the header line
    let (icon, icon_color) = tool_status_icon(args.status, args.is_running, args.spinner_index);
    let mut spans = vec![Span::styled(
        format!("{icon} "),
        Style::default().fg(icon_color),
    )];

    if let Some(k) = args.kind {
        if !matches!(k, ToolKind::Other) {
            spans.push(Span::styled(
                format!("{} ", tool_kind_icon(k)),
                Style::default().fg(Color::Cyan),
            ));
        }
    }

    spans.push(Span::styled(args.tool_name.to_string(), dim_gray));

    if args.is_running {
        if let Some(st) = args.status {
            spans.push(Span::styled(
                format!(" [{}]", tool_status_label(st)),
                dim_gray,
            ));
        }
    }

    // Format suffix (title and/or locations)
    let mut suffix_text = String::new();
    if let Some(t) = args.title {
        let clean_title = sanitize_title(t);
        if clean_title == args.tool_name {
            if !args.locations.is_empty() {
                suffix_text = format_locations(args.locations);
            }
        } else if !args.locations.is_empty() {
            let loc_str = format_locations(args.locations);
            if clean_title.contains(&loc_str) {
                suffix_text = clean_title;
            } else {
                suffix_text = format!("{clean_title}  {loc_str}");
            }
        } else {
            suffix_text = clean_title;
        }
    } else if !args.locations.is_empty() {
        suffix_text = format_locations(args.locations);
    }

    let timer_width = args.timer.map_or(0, UnicodeWidthStr::width);
    if !suffix_text.is_empty() {
        let prefix_width = spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum::<usize>()
            .saturating_add(timer_width);
        let suffix = title_suffix(Some(&suffix_text), usize::from(args.width), prefix_width);
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix, dim_gray));
        }
    }

    if let Some(t) = args.timer {
        spans.push(Span::styled(
            t.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }

    let header_line = Line::from(spans);

    match args.body {
        Some(ToolCallBody::Markdown(md)) => {
            let mut entry =
                crate::markdown_render::render_markdown(md, dim_gray, args.width, args.theme);
            entry.blocks.insert(
                0,
                MarkdownBlockData::Paragraph {
                    lines: vec![header_line],
                    height: 1,
                },
            );
            entry.total_height += 1;
            entry
        }
        Some(ToolCallBody::Yaml(yaml)) => {
            let mut lines = vec![header_line];
            for line in yaml.lines() {
                lines.extend(Tui::render_text_entry("", line, dim_gray, false));
            }
            RenderedEntry::from_lines(lines, args.width)
        }
        None => RenderedEntry::from_lines(vec![header_line], args.width),
    }
}

/// Payload for live tool updates dispatched to transcript items.
pub(crate) struct ToolUpdatePayload {
    pub id: String,
    pub markdown: Option<String>,
    pub status: Option<ToolStatus>,
    pub title: Option<String>,
    pub kind: Option<ToolKind>,
    pub locations: Option<Vec<ToolLocation>>,
    pub usage: Option<CompletionTokenUsage>,
}

/// Apply a tool update event to a transcript in place, handling ID lookup,
/// fallback binding, and minimal row creation.
pub(crate) fn apply_tool_event_update(
    transcript: &mut Vec<TranscriptItem>,
    update: ToolUpdatePayload,
    pending_seq: Option<usize>,
) {
    let matched_idx = transcript.iter().rposition(|item| {
        matches!(
            item,
            TranscriptItem::ToolCall {
                final_elapsed_ms: None,
                id: Some(ref i),
                ..
            } if i == &update.id
        )
    });
    if let Some(idx) = matched_idx {
        transcript[idx].apply_tool_update(
            update.markdown,
            update.status,
            update.title,
            update.kind,
            update.locations,
            update.usage,
        );
        return;
    }

    let already_completed = transcript.iter().any(|item| {
        matches!(
            item,
            TranscriptItem::ToolCall {
                id: Some(ref i),
                final_elapsed_ms: Some(_),
                ..
            } if i == &update.id
        )
    });
    if already_completed {
        return;
    }

    let fallback_idx = transcript.iter().rposition(|item| {
        matches!(
            item,
            TranscriptItem::ToolCall {
                id: None,
                final_elapsed_ms: None,
                ..
            }
        )
    });
    if let Some(idx) = fallback_idx {
        if let TranscriptItem::ToolCall {
            id: ref mut item_id,
            ..
        } = &mut transcript[idx]
        {
            *item_id = Some(update.id);
        }
        transcript[idx].apply_tool_update(
            update.markdown,
            update.status,
            update.title,
            update.kind,
            update.locations,
            update.usage,
        );
        return;
    }

    // Synthesize fallback row when Started was missing.
    // Use generic "tool" for tool_name so title is not rendered twice (P-DUPTITLE).
    transcript.push(TranscriptItem::ToolCall {
        tool_name: "tool".to_string(),
        body: update.markdown.map(crate::types::ToolCallBody::Markdown),
        seq: pending_seq,
        timestamp: Some(chrono::Utc::now()),
        id: Some(update.id),
        start_anchor: std::time::Instant::now(),
        final_elapsed_ms: None,
        rendered_cache: None,
        title: update.title,
        status: update
            .status
            .filter(|s| !matches!(s, ToolStatus::Completed | ToolStatus::Failed))
            .or(Some(ToolStatus::InProgress)),
        kind: update.kind,
        locations: update.locations.unwrap_or_default(),
        usage: update.usage,
    });
}

/// Complete a running tool call in the transcript, freezing its timer,
/// transitioning terminal status if active metadata exists, and invalidating render cache.
pub(crate) fn complete_tool_call(transcript: &mut [TranscriptItem], id: &str) {
    let matched_idx = transcript.iter().rposition(|item| {
        matches!(
            item,
            TranscriptItem::ToolCall {
                final_elapsed_ms: None,
                id: Some(ref i),
                ..
            } if i == id
        )
    });
    let fallback_idx = if matched_idx.is_none() {
        transcript.iter().rposition(|item| {
            matches!(
                item,
                TranscriptItem::ToolCall {
                    final_elapsed_ms: None,
                    ..
                }
            )
        })
    } else {
        None
    };
    if let Some(idx) = matched_idx.or(fallback_idx) {
        if let TranscriptItem::ToolCall {
            start_anchor,
            final_elapsed_ms,
            status,
            title,
            locations,
            kind,
            rendered_cache,
            ..
        } = &mut transcript[idx]
        {
            *final_elapsed_ms = Some(start_anchor.elapsed().as_millis() as u64);
            let has_live_updates = status.is_some()
                || title.is_some()
                || !locations.is_empty()
                || matches!(kind, Some(k) if !matches!(k, ToolKind::Other));
            if has_live_updates {
                *status = Some(ToolStatus::Completed);
            }
            *rendered_cache = None;
        }
    }
}

/// Mark a running tool call as failed in the transcript, freezing its timer,
/// transitioning terminal status if active metadata exists, and invalidating render cache.
pub(crate) fn fail_tool_call(transcript: &mut [TranscriptItem], id: &str) {
    let matched_idx = transcript.iter().rposition(|item| {
        matches!(
            item,
            TranscriptItem::ToolCall {
                final_elapsed_ms: None,
                id: Some(ref i),
                ..
            } if i == id
        )
    });
    let fallback_idx = if matched_idx.is_none() {
        transcript.iter().rposition(|item| {
            matches!(
                item,
                TranscriptItem::ToolCall {
                    final_elapsed_ms: None,
                    ..
                }
            )
        })
    } else {
        None
    };
    if let Some(idx) = matched_idx.or(fallback_idx) {
        if let TranscriptItem::ToolCall {
            start_anchor,
            final_elapsed_ms,
            status,
            title,
            locations,
            kind,
            rendered_cache,
            ..
        } = &mut transcript[idx]
        {
            *final_elapsed_ms = Some(start_anchor.elapsed().as_millis() as u64);
            let has_live_updates = status.is_some()
                || title.is_some()
                || !locations.is_empty()
                || matches!(kind, Some(k) if !matches!(k, ToolKind::Other));
            if has_live_updates {
                *status = Some(ToolStatus::Failed);
            }
            *rendered_cache = None;
        }
    }
}
