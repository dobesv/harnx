use crate::markdown_render::{render_markdown, RenderedEntry};
use crate::types::Tui;
use harnx_core::text::{split_think_blocks, ThinkSegment};
use ratatui::style::{Color, Modifier, Style};
use syntect::highlighting::Theme;

pub(crate) fn render_thought_text(text: &str, width: u16) -> RenderedEntry {
    let mut lines = Tui::render_text_entry(
        "",
        "Thinking",
        Style::default().add_modifier(Modifier::DIM),
        false,
    );
    lines.extend(Tui::render_text_entry(
        "",
        text,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM | Modifier::ITALIC),
        false,
    ));
    RenderedEntry::from_lines(lines, width)
}

pub(crate) fn render_assistant_text(
    text: &str,
    width: u16,
    theme: Option<&Theme>,
) -> RenderedEntry {
    let mut entry = RenderedEntry::default();
    for segment in split_think_blocks(text) {
        let rendered = match segment {
            ThinkSegment::Text(text) => render_markdown(text, Style::default(), width, theme),
            ThinkSegment::Thought(text) => render_thought_text(text, width),
        };
        entry.blocks.extend(rendered.blocks);
        entry.total_height += rendered.total_height;
        entry.links.extend(rendered.links);
    }
    entry
}
