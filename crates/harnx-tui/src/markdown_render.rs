use std::sync::LazyLock;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Cell, Paragraph, Row, Table, Widget, Wrap},
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SyntectColor, FontStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use unicode_width::UnicodeWidthStr;

/// Comes from <https://github.com/sharkdp/bat/raw/5e77ca37e89c873e4490b42ff556370dc5c6ba4f/assets/syntaxes.bin>
const SYNTAXES: &[u8] = include_bytes!("../../harnx-render/assets/syntaxes.bin");

static SYNTAX_SET: LazyLock<Option<SyntaxSet>> = LazyLock::new(|| {
    bincode::serde::decode_from_slice(SYNTAXES, bincode::config::legacy())
        .map(|(set, _): (SyntaxSet, usize)| set)
        .ok()
});

/// Language name mappings for syntax detection (e.g., "csharp" -> "C#")
static LANG_MAPS: LazyLock<std::collections::HashMap<String, String>> = LazyLock::new(|| {
    let mut m = std::collections::HashMap::new();
    m.insert("csharp".into(), "C#".into());
    m.insert("php".into(), "PHP Source".into());
    m
});

#[derive(Clone, Debug)]
pub enum MarkdownBlockData {
    Paragraph {
        lines: Vec<Line<'static>>,
        height: u16,
    },
    Table {
        header: Option<Vec<Cell<'static>>>,
        header_height: u16,
        rows: Vec<Vec<Cell<'static>>>,
        row_heights: Vec<u16>,
        col_widths: Vec<u16>,
        height: u16,
    },
}

#[derive(Clone, Debug, Default)]
pub struct RenderedEntry {
    pub blocks: Vec<MarkdownBlockData>,
    pub total_height: u16,
}

impl RenderedEntry {
    pub fn from_lines(lines: Vec<Line<'static>>, width: u16) -> RenderedEntry {
        if lines.is_empty() {
            return RenderedEntry::default();
        }
        let height = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16;
        let total_height = height;
        RenderedEntry {
            blocks: vec![MarkdownBlockData::Paragraph { lines, height }],
            total_height,
        }
    }
}

impl Widget for RenderedEntry {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.blocks.is_empty() {
            return;
        }

        let constraints: Vec<Constraint> = self
            .blocks
            .iter()
            .map(|block| {
                Constraint::Length(match block {
                    MarkdownBlockData::Paragraph { height, .. } => *height,
                    MarkdownBlockData::Table { height, .. } => *height,
                })
            })
            .collect();
        let areas = Layout::vertical(constraints).split(area);

        for (block, sub_area) in self.blocks.into_iter().zip(areas.iter()) {
            match block {
                MarkdownBlockData::Paragraph { lines, .. } => {
                    Paragraph::new(lines)
                        .wrap(Wrap { trim: false })
                        .render(*sub_area, buf);
                }
                MarkdownBlockData::Table {
                    header,
                    header_height,
                    rows,
                    row_heights,
                    col_widths,
                    ..
                } => {
                    let constraints: Vec<Constraint> = col_widths
                        .iter()
                        .map(|width| Constraint::Length(*width))
                        .collect();
                    let body_rows: Vec<Row<'static>> = rows
                        .into_iter()
                        .zip(row_heights)
                        .map(|(cells, height)| Row::new(cells).height(height))
                        .collect();
                    let mut table = Table::new(body_rows, constraints.clone());

                    if let Some(hdr) = header {
                        let header_row = Row::new(hdr)
                            .height(header_height)
                            .style(Style::default().add_modifier(Modifier::BOLD));
                        table = table.header(header_row);
                    }

                    table.block(Block::bordered()).render(*sub_area, buf);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct InlineState {
    style: Style,
    link: bool,
}

#[derive(Clone, Debug)]
struct ListState {
    next_index: usize,
    ordered: bool,
}

#[derive(Clone, Debug, Default)]
struct TableState {
    header: Option<Vec<Vec<Span<'static>>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    current_row: Vec<Vec<Span<'static>>>,
    current_row_widths: Vec<u16>,
    body_row_widths: Vec<Vec<u16>>,
    header_widths: Option<Vec<u16>>,
    current_cell_spans: Vec<Span<'static>>,
    current_cell_width: u16,
    in_header: bool,
}

pub fn render_markdown(
    text: &str,
    base_style: Style,
    width: u16,
    theme: Option<&Theme>,
) -> RenderedEntry {
    let mut blocks = Vec::new();
    let mut current_lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut inline_stack = vec![InlineState::default()];
    let mut heading_style: Option<Style> = None;
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut pending_blockquote_depth = 0usize;
    let mut pending_list_prefix: Option<(String, Style)> = None;
    let mut in_code_block = false;
    let mut code_block_lang: Option<String> = None;
    let mut table_state: Option<TableState> = None;

    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(text, options);

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    heading_style = Some(heading_style_for(level));
                }
                Tag::BlockQuote(_) => {
                    pending_blockquote_depth += 1;
                }
                Tag::List(start) => {
                    list_stack.push(ListState {
                        next_index: start.unwrap_or(1) as usize,
                        ordered: start.is_some(),
                    });
                }
                Tag::Item => {
                    flush_line(&mut current_lines, &mut current_spans);
                    pending_list_prefix = Some(next_list_prefix(&mut list_stack, base_style));
                }
                Tag::Emphasis => push_inline(
                    &mut inline_stack,
                    Style::default().add_modifier(Modifier::ITALIC),
                    false,
                ),
                Tag::Strong => push_inline(
                    &mut inline_stack,
                    Style::default().add_modifier(Modifier::BOLD),
                    false,
                ),
                Tag::Strikethrough => push_inline(
                    &mut inline_stack,
                    Style::default().add_modifier(Modifier::CROSSED_OUT),
                    false,
                ),
                Tag::Link { .. } => push_inline(
                    &mut inline_stack,
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                    true,
                ),
                Tag::CodeBlock(kind) => {
                    flush_line(&mut current_lines, &mut current_spans);
                    in_code_block = true;
                    code_block_lang = match kind {
                        CodeBlockKind::Fenced(lang) => Some(lang.into_string()),
                        CodeBlockKind::Indented => None,
                    };
                }
                Tag::Table(_) => {
                    flush_paragraph_block(
                        width,
                        &mut blocks,
                        &mut current_lines,
                        &mut current_spans,
                    );
                    table_state = Some(TableState::default());
                }
                Tag::TableHead => {
                    if let Some(state) = table_state.as_mut() {
                        state.in_header = true;
                    }
                }
                Tag::TableRow => {
                    if let Some(state) = table_state.as_mut() {
                        state.current_row = Vec::new();
                        state.current_row_widths = Vec::new();
                    }
                }
                Tag::TableCell => {
                    if let Some(state) = table_state.as_mut() {
                        state.current_cell_spans = Vec::new();
                        state.current_cell_width = 0;
                    }
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    flush_paragraph_block(
                        width,
                        &mut blocks,
                        &mut current_lines,
                        &mut current_spans,
                    );
                }
                TagEnd::Heading(_) => {
                    flush_paragraph_block(
                        width,
                        &mut blocks,
                        &mut current_lines,
                        &mut current_spans,
                    );
                    heading_style = None;
                }
                TagEnd::BlockQuote(_) => {
                    flush_paragraph_block(
                        width,
                        &mut blocks,
                        &mut current_lines,
                        &mut current_spans,
                    );
                    pending_blockquote_depth = pending_blockquote_depth.saturating_sub(1);
                }
                TagEnd::List(_) => {
                    flush_paragraph_block(
                        width,
                        &mut blocks,
                        &mut current_lines,
                        &mut current_spans,
                    );
                    list_stack.pop();
                }
                TagEnd::Item => {
                    flush_line(&mut current_lines, &mut current_spans);
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    pop_inline(&mut inline_stack);
                }
                TagEnd::CodeBlock => {
                    flush_paragraph_block(
                        width,
                        &mut blocks,
                        &mut current_lines,
                        &mut current_spans,
                    );
                    in_code_block = false;
                    code_block_lang = None;
                }
                TagEnd::TableHead => {
                    if let Some(state) = table_state.as_mut() {
                        state.header = Some(std::mem::take(&mut state.current_row));
                        state.header_widths = Some(std::mem::take(&mut state.current_row_widths));
                        state.in_header = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(state) = table_state.as_mut() {
                        state.rows.push(std::mem::take(&mut state.current_row));
                        state
                            .body_row_widths
                            .push(std::mem::take(&mut state.current_row_widths));
                    }
                }
                TagEnd::TableCell => {
                    if let Some(state) = table_state.as_mut() {
                        state
                            .current_row
                            .push(std::mem::take(&mut state.current_cell_spans));
                        state.current_row_widths.push(state.current_cell_width);
                    }
                }
                TagEnd::Table => {
                    if let Some(mut state) = table_state.take() {
                        state.in_header = false;
                        blocks.push(build_table_block(state, width));
                    }
                }
                _ => {}
            },
            Event::Text(content) => {
                if in_code_block {
                    // Apply syntect syntax highlighting to code block text
                    append_code_block_text(
                        &mut current_lines,
                        &mut current_spans,
                        &content,
                        code_block_lang.as_deref(),
                        base_style,
                        pending_blockquote_depth,
                        &mut pending_list_prefix,
                        theme,
                    );
                } else {
                    let span_style = resolve_span_style(base_style, &inline_stack, heading_style);
                    if let Some(state) = table_state.as_mut() {
                        append_text_to_cell(
                            &mut state.current_cell_spans,
                            &content,
                            span_style,
                            &mut state.current_cell_width,
                        );
                    } else {
                        append_text(
                            &mut current_lines,
                            &mut current_spans,
                            &content,
                            span_style,
                            pending_blockquote_depth,
                            &mut pending_list_prefix,
                        );
                    }
                }
            }
            Event::Code(content) => {
                let style = resolve_span_style(base_style, &inline_stack, heading_style)
                    .patch(Style::default().fg(Color::Cyan));
                if let Some(state) = table_state.as_mut() {
                    let owned = content.into_string();
                    let fragment_width: u16 = UnicodeWidthStr::width(owned.as_str())
                        .try_into()
                        .unwrap_or(u16::MAX);
                    state.current_cell_width =
                        state.current_cell_width.saturating_add(fragment_width);
                    state.current_cell_spans.push(Span::styled(owned, style));
                } else {
                    ensure_line_prefix(
                        &mut current_spans,
                        pending_blockquote_depth,
                        &mut pending_list_prefix,
                    );
                    current_spans.push(Span::styled(content.into_string(), style));
                }
            }
            Event::Html(content) | Event::InlineHtml(content) => {
                let style = resolve_span_style(base_style, &inline_stack, heading_style);
                if let Some(state) = table_state.as_mut() {
                    append_text_to_cell(
                        &mut state.current_cell_spans,
                        &content,
                        style,
                        &mut state.current_cell_width,
                    );
                } else {
                    append_text(
                        &mut current_lines,
                        &mut current_spans,
                        &content,
                        style,
                        pending_blockquote_depth,
                        &mut pending_list_prefix,
                    );
                }
            }
            Event::InlineMath(content) | Event::DisplayMath(content) => {
                let style = resolve_span_style(base_style, &inline_stack, heading_style)
                    .patch(Style::default().fg(Color::Magenta));
                if let Some(state) = table_state.as_mut() {
                    append_text_to_cell(
                        &mut state.current_cell_spans,
                        &content,
                        style,
                        &mut state.current_cell_width,
                    );
                } else {
                    append_text(
                        &mut current_lines,
                        &mut current_spans,
                        &content,
                        style,
                        pending_blockquote_depth,
                        &mut pending_list_prefix,
                    );
                }
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                let style = resolve_span_style(base_style, &inline_stack, heading_style);
                if let Some(state) = table_state.as_mut() {
                    state.current_cell_width = state.current_cell_width.saturating_add(4);
                    state
                        .current_cell_spans
                        .push(Span::styled(marker.to_string(), style));
                } else {
                    ensure_line_prefix(
                        &mut current_spans,
                        pending_blockquote_depth,
                        &mut pending_list_prefix,
                    );
                    current_spans.push(Span::styled(marker.to_string(), style));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(state) = table_state.as_mut() {
                    state.current_cell_spans.push(Span::styled(
                        "\n".to_string(),
                        resolve_span_style(base_style, &inline_stack, heading_style),
                    ));
                } else {
                    flush_line(&mut current_lines, &mut current_spans);
                }
            }
            Event::Rule => {
                flush_paragraph_block(width, &mut blocks, &mut current_lines, &mut current_spans);
                let line = Line::from(Span::styled("────".to_string(), base_style));
                let height = paragraph_height(std::slice::from_ref(&line), width);
                blocks.push(MarkdownBlockData::Paragraph {
                    lines: vec![line],
                    height,
                });
            }
            Event::FootnoteReference(content) => {
                let style = resolve_span_style(base_style, &inline_stack, heading_style)
                    .patch(Style::default().add_modifier(Modifier::UNDERLINED));
                if let Some(state) = table_state.as_mut() {
                    let owned = format!("[{content}]");
                    let fragment_width: u16 = UnicodeWidthStr::width(owned.as_str())
                        .try_into()
                        .unwrap_or(u16::MAX);
                    state.current_cell_width =
                        state.current_cell_width.saturating_add(fragment_width);
                    state.current_cell_spans.push(Span::styled(owned, style));
                } else {
                    ensure_line_prefix(
                        &mut current_spans,
                        pending_blockquote_depth,
                        &mut pending_list_prefix,
                    );
                    current_spans.push(Span::styled(format!("[{content}]"), style));
                }
            }
        }
    }

    flush_paragraph_block(width, &mut blocks, &mut current_lines, &mut current_spans);
    let total_height = blocks
        .iter()
        .map(|block| match block {
            MarkdownBlockData::Paragraph { height, .. } => *height,
            MarkdownBlockData::Table { height, .. } => *height,
        })
        .sum();

    RenderedEntry {
        blocks,
        total_height,
    }
}

fn push_inline(stack: &mut Vec<InlineState>, style: Style, link: bool) {
    let mut next = *stack.last().unwrap_or(&InlineState::default());
    next.style = next.style.patch(style);
    next.link |= link;
    stack.push(next);
}

fn pop_inline(stack: &mut Vec<InlineState>) {
    if stack.len() > 1 {
        stack.pop();
    }
}

fn resolve_span_style(
    base_style: Style,
    stack: &[InlineState],
    heading_style: Option<Style>,
) -> Style {
    let mut style = base_style;
    if let Some(heading) = heading_style {
        style = style.patch(heading);
    }
    style.patch(stack.last().copied().unwrap_or_default().style)
}

fn heading_style_for(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        HeadingLevel::H2 => Style::default().add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => {
            Style::default().add_modifier(Modifier::ITALIC)
        }
    }
}

fn next_list_prefix(list_stack: &mut [ListState], base_style: Style) -> (String, Style) {
    let list = list_stack
        .last_mut()
        .expect("list item without list context");
    if list.ordered {
        let prefix = format!("{}. ", list.next_index);
        list.next_index += 1;
        (prefix, base_style)
    } else {
        ("- ".to_string(), base_style)
    }
}

fn ensure_line_prefix(
    current_spans: &mut Vec<Span<'static>>,
    blockquote_depth: usize,
    pending_list_prefix: &mut Option<(String, Style)>,
) {
    if !current_spans.is_empty() {
        return;
    }

    if blockquote_depth > 0 {
        let prefix = "│ ".repeat(blockquote_depth);
        current_spans.push(Span::styled(prefix, Style::default().fg(Color::DarkGray)));
    }

    if let Some((prefix, style)) = pending_list_prefix.take() {
        current_spans.push(Span::styled(prefix, style));
    }
}

/// Append text content inside a code block, applying syntect syntax highlighting.
/// For each line in the content, find the appropriate syntax (by language or first-line heuristics),
/// run it through syntect's HighlightLines, and convert the styled ranges to ratatui Spans.
#[allow(clippy::too_many_arguments)]
fn append_code_block_text(
    current_lines: &mut Vec<Line<'static>>,
    current_spans: &mut Vec<Span<'static>>,
    text: &str,
    lang: Option<&str>,
    base_style: Style,
    blockquote_depth: usize,
    pending_list_prefix: &mut Option<(String, Style)>,
    theme: Option<&Theme>,
) {
    // Get syntax set and theme from statics
    let Some(syntax_set) = SYNTAX_SET.as_ref() else {
        // Fallback to plain dim text if syntax set failed to load
        let fallback_style = base_style.patch(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        );
        append_text(
            current_lines,
            current_spans,
            text,
            fallback_style,
            blockquote_depth,
            pending_list_prefix,
        );
        return;
    };

    let Some(theme) = theme else {
        // Fallback to plain dim text if theme failed to load
        let fallback_style = base_style.patch(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        );
        append_text(
            current_lines,
            current_spans,
            text,
            fallback_style,
            blockquote_depth,
            pending_list_prefix,
        );
        return;
    };

    // Find syntax by language name or extension
    let syntax: Option<&SyntaxReference> = lang.and_then(|l| find_syntax_by_name(syntax_set, l));

    // For the no-known-syntax path: detect from first line then reuse for all subsequent lines.
    // We also handle diff as a special case.
    let effective_syntax: Option<&SyntaxReference> = if syntax.is_some() {
        syntax
    } else {
        // Peek at first non-empty segment for first-line detection
        let first_seg = text.split('\n').next().unwrap_or("");
        if first_seg.starts_with("diff --git") || lang == Some("diff") {
            find_syntax_by_name(syntax_set, "diff")
        } else {
            find_syntax_by_first_line(syntax_set, first_seg)
        }
    };

    // Create a single HighlightLines for the whole block so multi-line parser state
    // (block comments, heredocs, strings) is preserved across lines.
    let mut highlighter: Option<HighlightLines> =
        effective_syntax.map(|s| HighlightLines::new(s, theme));

    // Process each line; use index to detect first segment without string comparison.
    for (seg_idx, segment) in text.split('\n').enumerate() {
        let needs_newline = !current_spans.is_empty() || !current_lines.is_empty();
        if needs_newline && seg_idx != 0 {
            flush_line(current_lines, current_spans);
        }
        ensure_line_prefix(current_spans, blockquote_depth, pending_list_prefix);

        if !segment.is_empty() {
            let spans = if let Some(hl) = highlighter.as_mut() {
                highlight_line_to_spans(segment, hl, syntax_set, base_style)
            } else {
                vec![Span::styled(segment.to_string(), base_style)]
            };
            current_spans.extend(spans);
        }
    }
}

/// Find a syntax by name or extension, with common language name mappings.
fn find_syntax_by_name<'a>(syntax_set: &'a SyntaxSet, name: &str) -> Option<&'a SyntaxReference> {
    let name_lower = name.to_ascii_lowercase();

    // Check language mapping (e.g., "csharp" -> "C#")
    if let Some(mapped) = LANG_MAPS.get(&name_lower) {
        if let Some(syntax) = syntax_set.find_syntax_by_name(mapped) {
            return Some(syntax);
        }
    }

    // Try by token/extension first (e.g., "rs", "py")
    syntax_set
        .find_syntax_by_token(name_lower.as_str())
        .or_else(|| syntax_set.find_syntax_by_extension(name_lower.as_str()))
        // Try by full name (case-insensitive-ish)
        .or_else(|| syntax_set.find_syntax_by_name(name))
        .or_else(|| {
            // Try uppercase version for languages like "C#"
            syntax_set.find_syntax_by_name(&name_lower.to_uppercase())
        })
        .or_else(|| {
            // Try title case
            let mut title = name_lower.clone();
            if let Some(first) = title.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            syntax_set.find_syntax_by_name(&title)
        })
}

/// Find a syntax by first-line content matching.
fn find_syntax_by_first_line<'a>(
    syntax_set: &'a SyntaxSet,
    line: &str,
) -> Option<&'a SyntaxReference> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Use syntect's built-in first-line detection
    syntax_set.find_syntax_by_first_line(line)
}

/// Highlight a single line using syntect and convert to ratatui Spans.
fn highlight_line_to_spans(
    line: &str,
    highlighter: &mut HighlightLines,
    syntax_set: &SyntaxSet,
    base_style: Style,
) -> Vec<Span<'static>> {
    match highlighter.highlight_line(line, syntax_set) {
        Ok(ranges) => {
            let mut spans = Vec::new();
            for (style, text) in ranges {
                let fg_color = convert_syntect_color(style.foreground);
                let mut ratatui_style = Style::default();
                if let Some(fg) = fg_color {
                    ratatui_style = ratatui_style.fg(fg);
                }
                if style.font_style.contains(FontStyle::BOLD) {
                    ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
                }
                if style.font_style.contains(FontStyle::UNDERLINE) {
                    ratatui_style = ratatui_style.add_modifier(Modifier::UNDERLINED);
                }
                // Merge with base style (base_style provides any unset attributes)
                ratatui_style = base_style.patch(ratatui_style);
                spans.push(Span::styled(text.to_string(), ratatui_style));
            }
            spans
        }
        Err(_) => {
            // Fallback: return unstyled text
            vec![Span::styled(line.to_string(), base_style)]
        }
    }
}

/// Convert a syntect Color to a ratatui Color (RGB).
fn convert_syntect_color(color: SyntectColor) -> Option<Color> {
    // Check for fully transparent (syntect default for unspecified)
    if color.a == 0 {
        return None;
    }
    Some(Color::Rgb(color.r, color.g, color.b))
}

/// Append text content with a given style, handling embedded newlines.
fn append_text(
    current_lines: &mut Vec<Line<'static>>,
    current_spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    blockquote_depth: usize,
    pending_list_prefix: &mut Option<(String, Style)>,
) {
    for segment in text.split('\n') {
        let needs_newline = !current_spans.is_empty() || !current_lines.is_empty();
        if needs_newline && segment != text.split('\n').next().unwrap_or_default() {
            flush_line(current_lines, current_spans);
        }
        ensure_line_prefix(current_spans, blockquote_depth, pending_list_prefix);
        if !segment.is_empty() {
            current_spans.push(Span::styled(segment.to_string(), style));
        }
    }
}

fn append_text_to_cell(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    current_width: &mut u16,
) {
    for (index, part) in text.split('\n').enumerate() {
        if index > 0 {
            spans.push(Span::styled("\n".to_string(), style));
            *current_width = 0;
        }
        if !part.is_empty() {
            let part_width = UnicodeWidthStr::width(part).try_into().unwrap_or(u16::MAX);
            *current_width = (*current_width).saturating_add(part_width);
            spans.push(Span::styled(part.to_string(), style));
        }
    }
}

fn flush_line(current_lines: &mut Vec<Line<'static>>, current_spans: &mut Vec<Span<'static>>) {
    if current_spans.is_empty() {
        return;
    }
    current_lines.push(Line::from(std::mem::take(current_spans)));
}

fn flush_paragraph_block(
    width: u16,
    blocks: &mut Vec<MarkdownBlockData>,
    current_lines: &mut Vec<Line<'static>>,
    current_spans: &mut Vec<Span<'static>>,
) {
    flush_line(current_lines, current_spans);
    if current_lines.is_empty() {
        return;
    }

    let lines = std::mem::take(current_lines);
    let height = paragraph_height(&lines, width);
    blocks.push(MarkdownBlockData::Paragraph { lines, height });
}

fn paragraph_height(lines: &[Line<'static>], width: u16) -> u16 {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width)
        .try_into()
        .unwrap_or(u16::MAX)
}

fn wrap_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::from(spans)];
    }

    if spans.is_empty() {
        return vec![Line::default()];
    }

    let mut plain_text = String::new();
    let mut span_ranges = Vec::with_capacity(spans.len());
    for span in spans {
        let byte_start = plain_text.len();
        plain_text.push_str(span.content.as_ref());
        let byte_end = plain_text.len();
        span_ranges.push((byte_start, byte_end, span.style));
    }

    let wrapped = textwrap::wrap(&plain_text, width as usize);
    if wrapped.is_empty() {
        return vec![Line::default()];
    }

    let mut search_start = 0usize;
    let mut lines = Vec::with_capacity(wrapped.len());
    for wrapped_line in wrapped {
        let wrapped_line = wrapped_line.as_ref();
        let relative_start = plain_text[search_start..]
            .find(wrapped_line)
            .unwrap_or_default();
        let line_start = search_start + relative_start;
        let line_end = line_start + wrapped_line.len();
        search_start = line_end;

        let line_spans = span_ranges
            .iter()
            .filter_map(|(span_start, span_end, style)| {
                let overlap_start = (*span_start).max(line_start);
                let overlap_end = (*span_end).min(line_end);
                if overlap_start < overlap_end {
                    Some(Span::styled(
                        plain_text[overlap_start..overlap_end].to_string(),
                        *style,
                    ))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(line_spans));
    }

    lines
}

fn build_table_block(state: TableState, width: u16) -> MarkdownBlockData {
    let column_count = state
        .header
        .as_ref()
        .map(|header| header.len())
        .unwrap_or_else(|| state.rows.first().map(|row| row.len()).unwrap_or(0));
    let mut col_widths = vec![0u16; column_count];

    if let Some(header_widths) = &state.header_widths {
        for (index, cell_width) in header_widths.iter().enumerate() {
            col_widths[index] = col_widths[index].max(*cell_width);
        }
    }
    for row_widths in &state.body_row_widths {
        for (index, cell_width) in row_widths.iter().enumerate() {
            col_widths[index] = col_widths[index].max(*cell_width);
        }
    }

    let natural_widths = col_widths.clone();
    shrink_table_widths(&mut col_widths, width, &natural_widths);

    let mut header_height = 0u16;
    let header = state.header.map(|header| {
        let mut max_line_count = 1u16;
        let cells = header
            .into_iter()
            .enumerate()
            .map(|(index, spans)| {
                let lines = wrap_spans(spans, col_widths[index]);
                max_line_count = max_line_count.max(lines.len().try_into().unwrap_or(u16::MAX));
                Cell::from(Text::from(lines))
            })
            .collect::<Vec<_>>();
        header_height = max_line_count;
        cells
    });
    let mut row_heights = Vec::with_capacity(state.rows.len());
    let rows = state
        .rows
        .into_iter()
        .map(|row| {
            let mut max_line_count = 1u16;
            let cells = row
                .into_iter()
                .enumerate()
                .map(|(index, spans)| {
                    let lines = wrap_spans(spans, col_widths[index]);
                    max_line_count = max_line_count.max(lines.len().try_into().unwrap_or(u16::MAX));
                    Cell::from(Text::from(lines))
                })
                .collect::<Vec<_>>();
            row_heights.push(max_line_count);
            cells
        })
        .collect::<Vec<_>>();
    let height =
        2 + if header.is_some() {
            header_height + 1
        } else {
            0
        } + row_heights.iter().copied().sum::<u16>();

    MarkdownBlockData::Table {
        header,
        header_height,
        rows,
        row_heights,
        col_widths,
        height,
    }
}

fn shrink_table_widths(col_widths: &mut [u16], available_width: u16, natural_widths: &[u16]) {
    if col_widths.is_empty() {
        return;
    }

    let min_widths: Vec<u16> = natural_widths
        .iter()
        .map(|width| if *width > 10 { 10 } else { *width })
        .collect();
    let separators = col_widths.len() as u16 + 1;
    let mut total_width: u16 = col_widths
        .iter()
        .copied()
        .sum::<u16>()
        .saturating_add(separators);
    if total_width <= available_width {
        return;
    }

    let available = available_width
        .saturating_sub(separators)
        .max(min_widths.iter().copied().sum::<u16>());
    let current_sum = col_widths.iter().copied().sum::<u16>().max(1);

    for (index, column_width) in col_widths.iter_mut().enumerate() {
        let proportional = ((*column_width as u32 * available as u32) / current_sum as u32) as u16;
        *column_width = proportional.max(min_widths[index]);
    }

    total_width = col_widths
        .iter()
        .copied()
        .sum::<u16>()
        .saturating_add(separators);
    while total_width > available_width {
        if let Some((index, _)) = col_widths
            .iter()
            .enumerate()
            .filter(|(index, w)| **w > min_widths[*index])
            .max_by_key(|(_, w)| **w)
        {
            col_widths[index] -= 1;
            total_width -= 1;
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        render_markdown, shrink_table_widths, wrap_spans, MarkdownBlockData, RenderedEntry,
    };
    use ratatui::{
        style::{Color, Modifier, Style},
        text::Span,
    };
    use syntect::highlighting::Theme;

    #[test]
    fn paragraph_renders_to_paragraph_block() {
        let rendered = render_markdown("plain text", Style::default(), 40, None);
        assert_eq!(rendered.blocks.len(), 1);
        match &rendered.blocks[0] {
            MarkdownBlockData::Paragraph { lines, height } => {
                assert_eq!(
                    lines[0]
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>(),
                    "plain text"
                );
                assert_eq!(*height, 1);
            }
            other => panic!("expected paragraph block, got {other:?}"),
        }
    }

    #[test]
    fn inline_styles_are_preserved() {
        let rendered = render_markdown("**bold** *italic* `code`", Style::default(), 80, None);
        let MarkdownBlockData::Paragraph { lines, .. } = &rendered.blocks[0] else {
            panic!("expected paragraph block");
        };
        let bold = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "bold")
            .unwrap();
        let italic = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "italic")
            .unwrap();
        let code = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "code")
            .unwrap();

        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(code.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn code_fence_renders_as_paragraph_without_fence_markers() {
        let rendered = render_markdown("```rust\nfn main() {}\n```", Style::default(), 80, None);
        assert_eq!(rendered.blocks.len(), 1);
        let MarkdownBlockData::Paragraph { lines, .. } = &rendered.blocks[0] else {
            panic!("expected paragraph block");
        };
        let texts: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(texts.iter().any(|line| line.contains("fn main() {}")));
        assert!(!texts.iter().any(|line| line.contains("```")));
    }

    #[test]
    fn gfm_table_renders_table_block_with_widths() {
        let rendered = render_markdown(
            "| alpha | beta | gamma |\n| --- | --- | --- |\n| one | three | seven |\n| two | four | six |",
            Style::default(),
            80,
            None,
        );
        assert_eq!(rendered.blocks.len(), 1);
        match &rendered.blocks[0] {
            MarkdownBlockData::Table {
                header,
                rows,
                col_widths,
                ..
            } => {
                assert!(header.is_some());
                assert_eq!(rows.len(), 2);
                assert_eq!(col_widths, &vec![5, 5, 5]);
            }
            other => panic!("expected table block, got {other:?}"),
        }
    }

    #[test]
    fn wrap_spans_single_span_no_wrap() {
        let lines = wrap_spans(vec![Span::raw("hello")], 10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "hello");
    }

    #[test]
    fn wrap_spans_single_span_wraps() {
        let lines = wrap_spans(vec![Span::raw("hello world")], 5);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines
                .iter()
                .map(|line| line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>())
                .collect::<Vec<_>>(),
            vec!["hello", "world"]
        );
    }

    #[test]
    fn wrap_spans_multi_span_wraps_at_boundary() {
        let left_style = Style::default().fg(Color::Yellow);
        let right_style = Style::default().fg(Color::Blue);
        let lines = wrap_spans(
            vec![
                Span::styled("hello ", left_style),
                Span::styled("world", right_style),
            ],
            6,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content.as_ref(), "hello");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(lines[1].spans[0].content.as_ref(), "world");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Blue));
    }

    #[test]
    fn wrap_spans_multi_span_wraps_mid_span() {
        let style = Style::default().fg(Color::Green);
        let lines = wrap_spans(vec![Span::styled("hello world", style)], 7);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines
                .iter()
                .map(|line| line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>())
                .collect::<Vec<_>>(),
            vec!["hello", "world"]
        );
        assert!(lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .all(|span| span.style.fg == Some(Color::Green)));
    }

    #[test]
    fn code_block_theme_parameter_controls_highlighting() {
        let dark = load_builtin_theme("monokai-extended");
        let light = load_builtin_theme("monokai-extended-light");
        let input = "```rust\nlet s = \"hi\";\n```";

        let dark_rendered = render_markdown(input, Style::default(), 80, Some(&dark));
        let light_rendered = render_markdown(input, Style::default(), 80, Some(&light));
        let fallback_rendered = render_markdown(input, Style::default(), 80, None);

        let dark_string = find_span(&dark_rendered, "hi");
        let light_string = find_span(&light_rendered, "hi");
        let fallback_string = find_span(&fallback_rendered, "hi");

        assert_ne!(dark_string.style.fg, light_string.style.fg);
        assert_ne!(dark_string.style, light_string.style);
        assert_eq!(fallback_string.style.fg, Some(Color::DarkGray));
        assert!(fallback_string.style.add_modifier.contains(Modifier::DIM));
    }

    fn load_builtin_theme(name: &str) -> Theme {
        let bytes: &[u8] = match name {
            "monokai-extended" => {
                include_bytes!("../../harnx-render/assets/monokai-extended.theme.bin")
            }
            "monokai-extended-light" => {
                include_bytes!("../../harnx-render/assets/monokai-extended-light.theme.bin")
            }
            other => panic!("unknown builtin theme {other}"),
        };

        bincode::serde::decode_from_slice(bytes, bincode::config::legacy())
            .map(|(theme, _): (Theme, usize)| theme)
            .expect("decode builtin theme")
    }

    fn find_span<'a>(rendered: &'a RenderedEntry, needle: &str) -> &'a Span<'static> {
        rendered
            .blocks
            .iter()
            .flat_map(|block| match block {
                MarkdownBlockData::Paragraph { lines, .. } => lines.iter(),
                MarkdownBlockData::Table { .. } => panic!("expected paragraph block"),
            })
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref().contains(needle))
            .unwrap_or_else(|| panic!("missing span containing {needle:?}"))
    }

    #[test]
    fn wrap_spans_empty() {
        let lines = wrap_spans(Vec::new(), 10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.is_empty());
    }

    #[test]
    fn wrap_spans_panic() {
        let spans = vec![ratatui::text::Span::raw("a\n💖")];
        let lines = wrap_spans(spans, 1);
        assert!(!lines.is_empty());
    }

    #[test]
    fn wrap_spans_zero_width() {
        let lines = wrap_spans(vec![Span::raw("hello world")], 0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "hello world");
    }

    #[test]
    fn gfm_table_long_content_wraps_and_height_correct() {
        let rendered = render_markdown(
            "| col |\n| --- |\n| this is a very long cell value! |",
            Style::default(),
            13,
            None,
        );
        assert_eq!(rendered.blocks.len(), 1);
        match &rendered.blocks[0] {
            MarkdownBlockData::Table { rows, height, .. } => {
                assert_eq!(rows.len(), 1);
                assert!(*height > rows.len() as u16 + 2);
            }
            other => panic!("expected table block, got {other:?}"),
        }
    }

    #[test]
    fn gfm_table_short_content_no_wrap() {
        let rendered = render_markdown("| ab |\n| --- |\n| cd |", Style::default(), 80, None);
        assert_eq!(rendered.blocks.len(), 1);
        match &rendered.blocks[0] {
            MarkdownBlockData::Table {
                col_widths,
                height,
                rows,
                ..
            } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(col_widths[0], 2);
                assert_eq!(*height, 5);
            }
            other => panic!("expected table block, got {other:?}"),
        }
    }

    #[test]
    fn total_height_sums_block_heights() {
        let rendered = render_markdown("first\n\nsecond", Style::default(), 80, None);
        let expected: u16 = rendered
            .blocks
            .iter()
            .map(|block| match block {
                MarkdownBlockData::Paragraph { height, .. } => *height,
                MarkdownBlockData::Table { height, .. } => *height,
            })
            .sum();
        assert_eq!(rendered.total_height, expected);
    }

    // ========================================
    // shrink_table_widths tests
    // ========================================

    #[test]
    fn shrink_table_widths_fits_no_change() {
        // 3 columns of width [5, 4, 5], available_width=80.
        // Table uses 5+4+5+4 separators = 18 < 80. No shrinkage needed.
        let mut col_widths = vec![5u16, 4, 5];
        let natural_widths = vec![5u16, 4, 5];
        shrink_table_widths(&mut col_widths, 80, &natural_widths);
        assert_eq!(col_widths, vec![5, 4, 5]);
    }

    #[test]
    fn shrink_table_widths_large_columns_shrink_proportionally() {
        // 2 columns natural widths [50, 50], available_width=30.
        // Min per column = 10 (since natural > 10).
        // Total natural = 100, separators = 3, available_for_content = 27.
        // Proportional: floor(50 * 27 / 100) = 13 per column.
        // Total = 13 + 13 + 3 = 29 ≤ 30. Each column >= min (10).
        let mut col_widths = vec![50u16, 50];
        let natural_widths = vec![50u16, 50];
        shrink_table_widths(&mut col_widths, 30, &natural_widths);
        assert_eq!(col_widths, vec![13, 13]);
        // Verify minimum constraint is respected
        assert!(col_widths[0] >= 10);
        assert!(col_widths[1] >= 10);
    }

    #[test]
    fn shrink_table_widths_small_columns_shrink_to_natural_min() {
        // 2 columns natural widths [5, 5], available_width=10.
        // Table needs 5+5+3=13 > 10. Min per column = 5 (since natural <= 10).
        // Loop can't shrink below min. Result should be [5, 5].
        let mut col_widths = vec![5u16, 5];
        let natural_widths = vec![5u16, 5];
        shrink_table_widths(&mut col_widths, 10, &natural_widths);
        assert_eq!(col_widths, vec![5, 5]);
    }

    #[test]
    fn shrink_table_widths_one_large_one_small() {
        // Natural widths [50, 5]. Available_width=20.
        // Min for col 0 = 10, min for col 1 = 5.
        // Proportional: total natural = 55, available_for_content = 20 - 3 = 17.
        // Col 0 gets floor(50*17/55) = 15, col 1 gets floor(5*17/55) = 1 → max(5) = 5.
        // Total = 15+5+3 = 23 > 20.
        // Trim widest (col 0 = 15 > 10) until total <= 20:
        // 14+5+3=22, 13+5+3=21, 12+5+3=20. Result: [12, 5].
        let mut col_widths = vec![50u16, 5];
        let natural_widths = vec![50u16, 5];
        shrink_table_widths(&mut col_widths, 20, &natural_widths);
        assert!(
            col_widths[0] >= 10,
            "col 0 should be >= 10, got {}",
            col_widths[0]
        );
        assert!(
            col_widths[1] >= 5,
            "col 1 should be >= 5, got {}",
            col_widths[1]
        );
        // Also verify the expected exact result
        assert_eq!(col_widths, vec![12, 5]);
    }

    #[test]
    fn shrink_table_widths_empty() {
        // Empty col_widths → no panic, nothing changes.
        let mut col_widths: Vec<u16> = vec![];
        let natural_widths: Vec<u16> = vec![];
        shrink_table_widths(&mut col_widths, 80, &natural_widths);
        assert!(col_widths.is_empty());
    }

    // ========================================
    // Additional wrap_spans tests
    // ========================================

    #[test]
    fn wrap_spans_unicode_multibyte_fits() {
        // Span containing "日本語" (3 CJK chars, each width 2 = total display width 6).
        // Call with width=10. Should return 1 line, content preserved.
        let lines = wrap_spans(vec![Span::raw("日本語")], 10);
        assert_eq!(lines.len(), 1);
        let content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(content, "日本語");
    }

    #[test]
    fn wrap_spans_long_word_no_break() {
        // Single span "supercalifragilistic" (20 chars) at width=10.
        // textwrap default: long words that exceed width are NOT broken (no hyphenation).
        // They may still be split across lines depending on textwrap's behavior.
        // The important assertion is that the full content is preserved across all lines.
        let lines = wrap_spans(vec![Span::raw("supercalifragilistic")], 10);
        // Collect all content from all lines
        let all_content: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert_eq!(all_content, "supercalifragilistic");
    }

    #[test]
    fn wrap_spans_explicit_newline_in_span() {
        // A span containing "line1\nline2" (with actual newline).
        // textwrap treats \n as a hard line break.
        // Assert the result has 2 lines.
        let lines = wrap_spans(vec![Span::raw("line1\nline2")], 10);
        assert_eq!(lines.len(), 2);
        let line_contents: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(line_contents, vec!["line1", "line2"]);
    }

    #[test]
    fn wrap_spans_repeated_substring_distinct_mapping() {
        // Two spans: Span::styled("aa", StyleA) and Span::styled("aa", StyleB).
        // Call with width=10 (no wrap needed).
        // Assert: result is 1 line with 2 spans, first "aa" has StyleA, second "aa" has StyleB.
        let style_a = Style::default().fg(Color::Red);
        let style_b = Style::default().fg(Color::Blue);
        let lines = wrap_spans(
            vec![Span::styled("aa", style_a), Span::styled("aa", style_b)],
            10,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[0].spans[0].content.as_ref(), "aa");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[0].spans[1].content.as_ref(), "aa");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Blue));
    }

    #[test]
    fn wrap_spans_exact_height_one_line_wraps() {
        // Use width=5, single span "hello world" (11 chars).
        // Assert exactly 2 lines returned, first line = "hello", second line = "world".
        let lines = wrap_spans(vec![Span::raw("hello world")], 5);
        assert_eq!(lines.len(), 2);
        let line_contents: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(line_contents, vec!["hello", "world"]);
    }

    // ========================================
    // Exact height test
    // ========================================

    #[test]
    fn gfm_table_height_exact_formula() {
        // Table with 1 header row and 2 body rows, all content short (≤ 5 chars each, width=80 — no wrapping).
        // Header height = 1, each row height = 1.
        // Expected height = 2 (borders) + 1 (header_height) + 1 (header separator) + 2 (2 body rows × 1) = 6.
        let rendered = render_markdown(
            "| col1 | col2 |\n| --- | --- |\n| abc | def |\n| ghi | jkl |",
            Style::default(),
            80,
            None,
        );
        assert_eq!(rendered.blocks.len(), 1);
        match &rendered.blocks[0] {
            MarkdownBlockData::Table { height, .. } => {
                assert_eq!(*height, 6);
            }
            other => panic!("expected table block, got {other:?}"),
        }
    }
}
#[test]
fn test_wrap_panic_3() {
    let spans = vec![ratatui::text::Span::raw("💖   ")];
    let lines = wrap_spans(spans, 2);
    println!("{:?}", lines);
}
