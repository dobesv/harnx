//! Runtime-note rendering: compact dim transcript rows and full-text detail
//! sections. Lives outside `render.rs` so that already-oversized file does
//! not keep growing.

use crate::markdown_render::RenderedEntry;
use crate::types::Tui;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Viewport-dependent display flags threaded through row renderers.
pub(super) struct TranscriptDisplay {
    pub(super) show_seq: bool,
    pub(super) show_ts: bool,
    pub(super) use_utc: bool,
    pub(super) width: u16,
}

/// Push a detail-view `key: value` field. Multi-line values get the key on
/// its own line followed by the raw lines; single-line values stay inline.
/// `render_entry_detail`'s inline `push_field!` macro delegates here.
pub(super) fn push_detail_field(
    lines: &mut Vec<Line<'static>>,
    label_style: Style,
    key: &str,
    value: &str,
) {
    if value.contains('\n') {
        lines.push(Line::from(Span::styled(format!("{key}:"), label_style)));
        for line in value.lines() {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled(format!("{key}: "), label_style),
            Span::raw(value.to_string()),
        ]));
    }
}

impl Tui {
    /// Append a meta suffix span to the first line of an entry, if any.
    pub(super) fn append_meta_suffix(lines: &mut [Line<'static>], suffix: Option<Span<'static>>) {
        if let (Some(suffix), Some(first_line)) = (suffix, lines.first_mut()) {
            first_line.spans.push(suffix);
        }
    }

    /// Render a runtime-note transcript row: model-context machinery, not
    /// conversation, so a dim compact one-liner instead of cyan user text.
    /// The full raw note stays available in the detail view.
    pub(super) fn render_runtime_note_entry(
        compact: &str,
        seq: Option<usize>,
        timestamp: Option<chrono::DateTime<chrono::Utc>>,
        display: &TranscriptDisplay,
    ) -> RenderedEntry {
        let mut lines =
            Self::render_text_entry("", compact, super::dim_style(Color::DarkGray), false);
        let suffix = Self::render_meta_suffix(
            seq,
            timestamp,
            display.show_seq,
            display.show_ts,
            display.use_utc,
        );
        Self::append_meta_suffix(&mut lines, suffix);
        RenderedEntry::from_lines(lines, display.width)
    }

    /// Detail section shared by user text, assistant text, and runtime
    /// notes: heading, optional seq/timestamp, then the full text.
    /// `message` bundles `(text, seq, timestamp)`.
    pub(super) fn push_message_detail(
        lines: &mut Vec<Line<'static>>,
        label_style: Style,
        label: &str,
        message: (&str, Option<usize>, Option<chrono::DateTime<chrono::Utc>>),
    ) {
        let (text, seq, timestamp) = message;
        lines.push(Line::from(Span::styled(label.to_string(), label_style)));
        if let Some(s) = seq {
            push_detail_field(lines, label_style, "seq", &s.to_string());
        }
        if let Some(ts) = timestamp {
            push_detail_field(lines, label_style, "timestamp", &ts.to_rfc3339());
        }
        push_detail_field(lines, label_style, "text", text);
    }
}
