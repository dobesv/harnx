//! Keep session-wait labels inline with their status, kind and timer.

use super::{tool_kind_icon, tool_status_icon, tool_status_label, RenderToolCallArgs};
use crate::markdown_render::{render_markdown, MarkdownBlockData, RenderedEntry};
use crate::types::ToolCallBody;
use harnx_core::event::ToolKind;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

pub(super) fn render(args: &RenderToolCallArgs<'_>, style: Style) -> Option<RenderedEntry> {
    render_without_header(args, style).or_else(|| render_inline(args, style))
}

fn render_without_header(args: &RenderToolCallArgs<'_>, style: Style) -> Option<RenderedEntry> {
    // Preserve the existing template-only layout when there is no metadata.
    let has_metadata = args.title.is_some()
        || matches!(args.kind, Some(kind) if kind != ToolKind::Other)
        || !args.locations.is_empty()
        || args.is_running
        || args.timer.is_some();
    match args.body {
        Some(ToolCallBody::Markdown(markdown)) if !has_metadata => {
            Some(render_markdown(markdown, style, args.width, args.theme))
        }
        _ => None,
    }
}

fn render_inline(args: &RenderToolCallArgs<'_>, style: Style) -> Option<RenderedEntry> {
    // Native session_wait only observes: it emits no live Markdown updates.
    // Restrict this layout to its identifying label, keeping arbitrary tool
    // progress bodies attached to their normal tool-name header.
    if !is_wait_label(args) {
        return None;
    }
    let Some(ToolCallBody::Markdown(markdown)) = args.body else {
        return None;
    };
    if markdown.contains(['\r', '\n']) {
        return None;
    }
    let mut line = status_prefix(args, style);
    let timer = Span::styled(args.timer.unwrap_or_default().to_string(), style);
    let available = usize::from(args.width).checked_sub(line.width() + timer.width())?;
    if available == 0 {
        return None;
    }
    let mut entry = render_markdown(markdown, style, available.try_into().ok()?, args.theme);
    let [MarkdownBlockData::Paragraph { lines, height: 1 }] = entry.blocks.as_mut_slice() else {
        return None;
    };
    let [body] = lines.as_mut_slice() else {
        return None;
    };
    line.spans.append(&mut body.spans);
    line.spans.push(timer);
    let mut rendered = RenderedEntry::from_lines(vec![line], args.width);
    rendered.links = entry.links;
    Some(rendered)
}

fn status_prefix(args: &RenderToolCallArgs<'_>, style: Style) -> Line<'static> {
    let (icon, color) = tool_status_icon(args.status, args.is_running, args.spinner_index);
    let mut line = Line::from(Span::styled(format!("{icon} "), Style::default().fg(color)));
    if let Some(kind) = args.kind.filter(|kind| *kind != ToolKind::Other) {
        line.spans.push(Span::styled(
            format!("{} ", tool_kind_icon(kind)),
            Style::default().fg(Color::Cyan),
        ));
    }
    if let Some(status) = args.status.filter(|_| args.is_running) {
        line.spans.push(Span::styled(
            format!("[{}] ", tool_status_label(status)),
            style,
        ));
    }
    line
}

fn is_wait_label(args: &RenderToolCallArgs<'_>) -> bool {
    let Some(ToolCallBody::Markdown(markdown)) = args.body else {
        return false;
    };
    let wait = harnx_toolset::SUBAGENT_SESSION_WAIT_TOOL;
    let is_wait = args.tool_name == wait || args.tool_name.ends_with(&format!("_{wait}"));
    is_wait
        && args.title.is_none()
        && args.locations.is_empty()
        && markdown.starts_with("@ ")
        && markdown.contains("] wait ")
}

#[cfg(test)]
mod tests;
