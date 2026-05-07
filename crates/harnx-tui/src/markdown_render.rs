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

/// Monokai Extended dark theme (bincode-encoded)
const THEME_BYTES: &[u8] = include_bytes!("../../harnx-render/assets/monokai-extended.theme.bin");

static SYNTAX_SET: LazyLock<Option<SyntaxSet>> = LazyLock::new(|| {
    bincode::serde::decode_from_slice(SYNTAXES, bincode::config::legacy())
        .map(|(set, _): (SyntaxSet, usize)| set)
        .ok()
});

static THEME: LazyLock<Option<Theme>> = LazyLock::new(|| {
    bincode::serde::decode_from_slice(THEME_BYTES, bincode::config::legacy())
        .map(|(theme, _): (Theme, usize)| theme)
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
        rows: Vec<Vec<Cell<'static>>>,
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
                    rows,
                    col_widths,
                    ..
                } => {
                    let constraints: Vec<Constraint> = col_widths
                        .iter()
                        .map(|width| Constraint::Length(*width))
                        .collect();
                    let body_rows: Vec<Row<'static>> = rows.into_iter().map(Row::new).collect();
                    let mut table = Table::new(body_rows, constraints.clone());

                    if let Some(hdr) = header {
                        let header_row =
                            Row::new(hdr).style(Style::default().add_modifier(Modifier::BOLD));
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
    header: Option<Vec<Cell<'static>>>,
    rows: Vec<Vec<Cell<'static>>>,
    current_row: Vec<Cell<'static>>,
    current_row_widths: Vec<u16>,
    body_row_widths: Vec<Vec<u16>>,
    header_widths: Option<Vec<u16>>,
    current_cell_spans: Vec<Span<'static>>,
    current_cell_width: u16,
    in_header: bool,
}

pub fn render_markdown(text: &str, base_style: Style, width: u16) -> RenderedEntry {
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
                        let cell = Cell::from(Text::from(Line::from(std::mem::take(
                            &mut state.current_cell_spans,
                        ))));
                        state.current_row.push(cell);
                        state
                            .current_row_widths
                            .push(state.current_cell_width.max(3));
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
fn append_code_block_text(
    current_lines: &mut Vec<Line<'static>>,
    current_spans: &mut Vec<Span<'static>>,
    text: &str,
    lang: Option<&str>,
    base_style: Style,
    blockquote_depth: usize,
    pending_list_prefix: &mut Option<(String, Style)>,
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

    let Some(theme) = THEME.as_ref() else {
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

fn build_table_block(state: TableState, width: u16) -> MarkdownBlockData {
    let column_count = state
        .header
        .as_ref()
        .map(|header| header.len())
        .unwrap_or_else(|| state.rows.first().map(|row| row.len()).unwrap_or(0));
    let mut col_widths = vec![3u16; column_count];

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

    shrink_table_widths(&mut col_widths, width);

    let height = if state.header.is_some() {
        1 + 1 + state.rows.len() as u16 + 2
    } else {
        state.rows.len() as u16 + 2
    };

    MarkdownBlockData::Table {
        header: state.header,
        rows: state.rows,
        col_widths,
        height,
    }
}

fn shrink_table_widths(col_widths: &mut [u16], width: u16) {
    if col_widths.is_empty() {
        return;
    }

    let separators = col_widths.len() as u16 + 1;
    let mut total_width: u16 = col_widths
        .iter()
        .copied()
        .sum::<u16>()
        .saturating_add(separators);
    if total_width <= width {
        return;
    }

    let available = width
        .saturating_sub(separators)
        .max((col_widths.len() as u16) * 3);
    let current_sum = col_widths.iter().copied().sum::<u16>().max(1);

    for column_width in col_widths.iter_mut() {
        let proportional = ((*column_width as u32 * available as u32) / current_sum as u32) as u16;
        *column_width = proportional.max(3);
    }

    total_width = col_widths
        .iter()
        .copied()
        .sum::<u16>()
        .saturating_add(separators);
    while total_width > width {
        if let Some((index, _)) = col_widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 3)
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
    use super::{render_markdown, MarkdownBlockData};
    use ratatui::style::{Color, Modifier, Style};

    #[test]
    fn paragraph_renders_to_paragraph_block() {
        let rendered = render_markdown("plain text", Style::default(), 40);
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
        let rendered = render_markdown("**bold** *italic* `code`", Style::default(), 80);
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
        let rendered = render_markdown("```rust\nfn main() {}\n```", Style::default(), 80);
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
    fn total_height_sums_block_heights() {
        let rendered = render_markdown("first\n\nsecond", Style::default(), 80);
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
}
