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

static SYNTAX_SET: LazyLock<Option<SyntaxSet>> =
    LazyLock::new(|| syntect::dumps::from_uncompressed_data::<SyntaxSet>(SYNTAXES).ok());

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedLink {
    pub text: String,
    pub url: String,
}

#[derive(Clone, Debug, Default)]
pub struct RenderedEntry {
    pub blocks: Vec<MarkdownBlockData>,
    pub total_height: u16,
    pub links: Vec<ExtractedLink>,
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
            links: Vec::new(),
        }
    }

    pub fn reverse_style(&mut self) {
        let reverse = Style::default().add_modifier(Modifier::REVERSED);
        for block in &mut self.blocks {
            block.reverse_style(reverse);
        }
    }
}

impl MarkdownBlockData {
    fn reverse_style(&mut self, reverse: Style) {
        match self {
            MarkdownBlockData::Paragraph { lines, .. } => reverse_lines(lines, reverse),
            MarkdownBlockData::Table { rows, header, .. } => {
                reverse_table(rows, header.as_mut(), reverse);
            }
        }
    }
}

fn reverse_lines(lines: &mut [Line<'static>], reverse: Style) {
    for line in lines {
        line.style = line.style.patch(reverse);
        for span in &mut line.spans {
            span.style = span.style.patch(reverse);
        }
    }
}

fn reverse_table(
    rows: &mut [Vec<Cell<'static>>],
    header: Option<&mut Vec<Cell<'static>>>,
    reverse: Style,
) {
    if let Some(header) = header {
        reverse_cells(header, reverse);
    }
    for row in rows {
        reverse_cells(row, reverse);
    }
}

fn reverse_cells(cells: &mut [Cell<'static>], reverse: Style) {
    for cell in cells {
        *cell = cell.clone().style(reverse);
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

const MIN_WRAPPED_CONTENT_WIDTH: usize = 4;
const TASK_MARKER_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug, Default)]
struct InlineState {
    style: Style,
    link: bool,
}

#[derive(Clone, Debug)]
struct ListState {
    next_index: usize,
    ordered: bool,
    emitted_any: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockKind {
    Paragraph,
    Heading,
    List,
    CodeBlock,
    BlockQuote,
    Table,
    Rule,
    HtmlBlock,
    TightInline,
}

#[derive(Clone, Debug)]
struct ListItemContext {
    nest_spaces: usize,
    pending_marker: Option<(String, Style)>,
    content_offset: usize,
    has_emitted_block: bool,
    last_block_kind: Option<BlockKind>,
}

#[derive(Clone, Debug)]
enum Container {
    BlockQuote {
        has_emitted_block: bool,
        last_block_kind: Option<BlockKind>,
    },
    List(ListState),
    ListItem(ListItemContext),
}

#[derive(Clone, Debug)]
enum PrefixAtom {
    Bar,
    Space(usize),
    Marker(String, Style),
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

impl TableState {
    fn handle_start(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::TableHead => self.in_header = true,
            Tag::TableRow => {
                self.current_row.clear();
                self.current_row_widths.clear();
            }
            Tag::TableCell => {
                self.current_cell_spans.clear();
                self.current_cell_width = 0;
            }
            _ => {}
        }
    }

    fn handle_end(&mut self, tag_end: TagEnd) -> bool {
        match tag_end {
            TagEnd::TableHead => {
                self.header = Some(std::mem::take(&mut self.current_row));
                self.header_widths = Some(std::mem::take(&mut self.current_row_widths));
                self.in_header = false;
                false
            }
            TagEnd::TableRow => {
                self.rows.push(std::mem::take(&mut self.current_row));
                self.body_row_widths
                    .push(std::mem::take(&mut self.current_row_widths));
                false
            }
            TagEnd::TableCell => {
                self.current_row
                    .push(std::mem::take(&mut self.current_cell_spans));
                self.current_row_widths.push(self.current_cell_width);
                false
            }
            TagEnd::Table => {
                self.in_header = false;
                true
            }
            _ => false,
        }
    }

    fn push_text(&mut self, text: &str, style: Style) {
        append_text_to_cell(
            &mut self.current_cell_spans,
            text,
            style,
            &mut self.current_cell_width,
        );
    }

    fn push_inline_span(&mut self, text: String, style: Style) {
        let fragment_width: u16 = UnicodeWidthStr::width(text.as_str())
            .try_into()
            .unwrap_or(u16::MAX);
        self.current_cell_width = self.current_cell_width.saturating_add(fragment_width);
        self.current_cell_spans.push(Span::styled(text, style));
    }

    fn push_newline(&mut self, style: Style) {
        self.current_cell_spans
            .push(Span::styled("\n".to_string(), style));
    }
}

#[derive(Clone, Debug, Default)]
struct BlockTracker {
    container_stack: Vec<Container>,
    top_has_emitted: bool,
}

fn tracker_is_top_level(tracker: &BlockTracker) -> bool {
    tracker.container_stack.is_empty()
}

fn tracker_blockquote_depth(container_stack: &[Container]) -> usize {
    container_stack
        .iter()
        .filter(|c| matches!(c, Container::BlockQuote { .. }))
        .count()
}

fn tracker_needs_separator_before(tracker: &BlockTracker, next_kind: BlockKind) -> Option<usize> {
    let bq_depth = tracker_blockquote_depth(&tracker.container_stack);
    match tracker.container_stack.last() {
        None if tracker.top_has_emitted => Some(0),
        Some(Container::BlockQuote {
            has_emitted_block: true,
            ..
        }) => Some(bq_depth),
        Some(Container::ListItem(item))
            if next_kind != BlockKind::List
                && item.has_emitted_block
                && item.last_block_kind != Some(BlockKind::TightInline) =>
        {
            Some(bq_depth)
        }
        _ => None,
    }
}

fn tracker_needs_direct_nested_list_marker(tracker: &BlockTracker, next_kind: BlockKind) -> bool {
    next_kind == BlockKind::List
        && matches!(
            tracker.container_stack.last(),
            Some(Container::ListItem(item)) if item.pending_marker.is_some()
        )
}

fn tracker_record_emitted(tracker: &mut BlockTracker, kind: BlockKind) {
    match tracker.container_stack.last_mut() {
        None => tracker.top_has_emitted = true,
        Some(Container::BlockQuote {
            has_emitted_block,
            last_block_kind,
        }) => {
            *has_emitted_block = true;
            *last_block_kind = Some(kind);
        }
        Some(Container::ListItem(item)) => {
            item.has_emitted_block = true;
            item.last_block_kind = Some(kind);
            mark_innermost_list_emitted(&mut tracker.container_stack);
        }
        Some(Container::List(list)) => {
            list.emitted_any = true;
        }
    }
}

fn mark_innermost_list_emitted(container_stack: &mut [Container]) {
    for container in container_stack.iter_mut().rev() {
        if let Container::List(list) = container {
            list.emitted_any = true;
            break;
        }
    }
}

fn tracker_push_item(tracker: &mut BlockTracker, base_style: Style) {
    let nest_spaces = current_segment_list_depth(&tracker.container_stack) * 2;
    let marker = tracker
        .container_stack
        .iter_mut()
        .rev()
        .find_map(|c| match c {
            Container::List(list) => Some(list),
            _ => None,
        })
        .map(|list| next_list_prefix(list, base_style))
        .unwrap_or_else(|| ("- ".to_string(), base_style));
    let content_offset = UnicodeWidthStr::width(marker.0.as_str());
    tracker
        .container_stack
        .push(Container::ListItem(ListItemContext {
            nest_spaces,
            pending_marker: Some(marker),
            content_offset,
            has_emitted_block: false,
            last_block_kind: None,
        }));
}

fn tracker_record_popped_item(
    container_stack: &mut [Container],
    popped: Option<Container>,
    force_emitted: bool,
) {
    let emitted = match popped {
        Some(Container::ListItem(item)) => item.has_emitted_block,
        _ => false,
    };
    if emitted || force_emitted {
        mark_innermost_list_emitted(container_stack);
    }
}

fn tracker_try_attach_task_marker(
    tracker: &mut BlockTracker,
    marker: &str,
    base_style: Style,
) -> bool {
    let Some(Container::ListItem(item)) = tracker.container_stack.last_mut() else {
        return false;
    };
    let Some((prefix, prefix_style)) = item.pending_marker.as_mut() else {
        return false;
    };
    prefix.push_str(marker);
    *prefix_style = base_style;
    item.content_offset += TASK_MARKER_WIDTH;
    true
}

#[derive(Clone, Debug)]
struct PendingSeparator {
    stack_depth: usize,
    line: Line<'static>,
}

struct BlockEmitter {
    base_style: Style,
    width: u16,
    blocks: Vec<MarkdownBlockData>,
    current_lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    pending_separator: Option<PendingSeparator>,
    emitted_in_current_leaf: bool,
}

impl BlockEmitter {
    fn new(base_style: Style, width: u16) -> Self {
        Self {
            base_style,
            width,
            blocks: Vec::new(),
            current_lines: Vec::new(),
            current_spans: Vec::new(),
            pending_separator: None,
            emitted_in_current_leaf: false,
        }
    }

    fn clear_pending_separator_at(&mut self, stack_depth: usize) {
        if self
            .pending_separator
            .as_ref()
            .is_some_and(|sep| sep.stack_depth == stack_depth)
        {
            self.pending_separator = None;
        }
    }

    fn materialize_pending_separator(&mut self) {
        let Some(sep) = self.pending_separator.take() else {
            return;
        };
        if sep.stack_depth == 0 {
            self.flush_paragraph_block();
            let height = paragraph_height(std::slice::from_ref(&sep.line), self.width);
            self.blocks.push(MarkdownBlockData::Paragraph {
                lines: vec![sep.line],
                height,
            });
        } else {
            self.current_lines.push(sep.line);
        }
    }

    fn push_unprefixed_or_prepare(
        &mut self,
        tracker: &mut BlockTracker,
        content: Vec<Span<'static>>,
    ) -> Option<(Vec<Span<'static>>, Vec<Span<'static>>, Vec<Span<'static>>)> {
        self.materialize_pending_separator();
        if tracker.container_stack.is_empty() {
            let line = if content.is_empty() {
                Line::default()
            } else {
                Line::from(content)
            };
            self.current_lines.push(line);
            self.emitted_in_current_leaf = true;
            return None;
        }
        let (first_prefix, cont_prefix) = build_line_prefixes(
            &mut tracker.container_stack,
            self.base_style,
            self.width,
            true,
        );
        Some((first_prefix, cont_prefix, content))
    }

    fn emit_empty_item_marker_if_pending(&mut self, tracker: &mut BlockTracker) -> bool {
        let emit_empty = matches!(
            tracker.container_stack.last(),
            Some(Container::ListItem(item)) if item.pending_marker.is_some()
        );
        if emit_empty {
            self.emit_prefixed_spans(tracker, Vec::new());
        }
        emit_empty
    }

    fn emit_prefixed_spans(&mut self, tracker: &mut BlockTracker, content: Vec<Span<'static>>) {
        let Some((first_prefix, cont_prefix, content)) =
            self.push_unprefixed_or_prepare(tracker, content)
        else {
            return;
        };
        let wrapped_lines = wrap_prefixed_spans(first_prefix, cont_prefix, content, self.width);
        self.current_lines.extend(wrapped_lines);
        self.emitted_in_current_leaf = true;
    }

    fn emit_prefixed_code_line(&mut self, tracker: &mut BlockTracker, content: Vec<Span<'static>>) {
        let Some((first_prefix, cont_prefix, content)) =
            self.push_unprefixed_or_prepare(tracker, content)
        else {
            return;
        };
        if content.is_empty()
            && tracker_blockquote_depth(&tracker.container_stack) == 0
            && !first_prefix_has_marker(&first_prefix)
        {
            self.current_lines.push(Line::default());
            self.emitted_in_current_leaf = true;
            return;
        }
        let fits_single_line = self.width == 0
            || spans_width(&first_prefix) + spans_width(&content) <= self.width as usize;
        if fits_single_line {
            let mut spans = first_prefix;
            spans.extend(content);
            self.current_lines.push(Line::from(spans));
        } else {
            let wrapped_lines =
                wrap_prefixed_code_spans(first_prefix, cont_prefix, content, self.width);
            self.current_lines.extend(wrapped_lines);
        }
        self.emitted_in_current_leaf = true;
    }

    fn flush_current_line(&mut self, tracker: &mut BlockTracker) {
        if self.current_spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.current_spans);
        self.emit_prefixed_spans(tracker, spans);
    }

    fn flush_paragraph_block(&mut self) {
        if self.current_lines.is_empty() {
            return;
        }
        let lines = std::mem::take(&mut self.current_lines);
        let height = paragraph_height(&lines, self.width);
        self.blocks
            .push(MarkdownBlockData::Paragraph { lines, height });
    }

    fn append_inline_text(&mut self, tracker: &mut BlockTracker, text: &str, style: Style) {
        for (seg_idx, segment) in text.split('\n').enumerate() {
            if seg_idx > 0 {
                self.flush_current_line(tracker);
            }
            if !segment.is_empty() {
                self.current_spans
                    .push(Span::styled(segment.to_string(), style));
            }
        }
    }

    fn finish(mut self, tracker: &mut BlockTracker, links: Vec<ExtractedLink>) -> RenderedEntry {
        self.flush_current_line(tracker);
        self.flush_paragraph_block();
        let total_height = self
            .blocks
            .iter()
            .map(|block| match block {
                MarkdownBlockData::Paragraph { height, .. } => *height,
                MarkdownBlockData::Table { height, .. } => *height,
            })
            .sum();

        RenderedEntry {
            blocks: self.blocks,
            total_height,
            links,
        }
    }
}

struct MarkdownRenderer<'a> {
    emitter: BlockEmitter,
    theme: Option<&'a Theme>,
    inline_stack: Vec<InlineState>,
    heading_style: Option<Style>,
    tracker: BlockTracker,
    in_code_block: bool,
    code_block_lang: Option<String>,
    code_block_buffer: String,
    table_state: Option<TableState>,
    links: Vec<ExtractedLink>,
    current_link: Option<(String, String)>,
}

impl<'a> MarkdownRenderer<'a> {
    fn new(base_style: Style, width: u16, theme: Option<&'a Theme>) -> Self {
        Self {
            emitter: BlockEmitter::new(base_style, width),
            theme,
            inline_stack: vec![InlineState::default()],
            heading_style: None,
            tracker: BlockTracker::default(),
            in_code_block: false,
            code_block_lang: None,
            code_block_buffer: String::new(),
            table_state: None,
            links: Vec::new(),
            current_link: None,
        }
    }

    fn begin_block(&mut self, next_kind: BlockKind) {
        if self.table_state.is_some() {
            return;
        }
        if !self.emitter.current_spans.is_empty() {
            self.emitter.flush_current_line(&mut self.tracker);
            tracker_record_emitted(&mut self.tracker, BlockKind::TightInline);
        }
        if tracker_needs_direct_nested_list_marker(&self.tracker, next_kind) {
            // Item starts directly with a nested list: emit its marker line first.
            self.emitter
                .emit_prefixed_spans(&mut self.tracker, Vec::new());
        }
        if let Some(bq_depth) = tracker_needs_separator_before(&self.tracker, next_kind) {
            let line = separator_line(
                &self.tracker.container_stack,
                bq_depth,
                self.emitter.base_style,
                self.emitter.width,
            );
            self.emitter.pending_separator = Some(PendingSeparator {
                stack_depth: self.tracker.container_stack.len(),
                line,
            });
        }
    }

    fn flush_code_block(&mut self) {
        let raw = std::mem::take(&mut self.code_block_buffer);
        let lang = self.code_block_lang.take();
        self.in_code_block = false;

        if raw.is_empty() {
            self.emitter
                .clear_pending_separator_at(self.tracker.container_stack.len());
            return;
        }

        let code_lines = split_code_lines(&raw);
        let rendered_lines = highlight_code_lines(
            &code_lines,
            lang.as_deref(),
            self.emitter.base_style,
            self.theme,
        );
        for spans in rendered_lines {
            self.emitter
                .emit_prefixed_code_line(&mut self.tracker, spans);
        }

        tracker_record_emitted(&mut self.tracker, BlockKind::CodeBlock);
        if tracker_is_top_level(&self.tracker) {
            self.emitter.flush_paragraph_block();
        }
    }

    fn handle_event(&mut self, event: Event<'_>) {
        let span_style = resolve_span_style(
            self.emitter.base_style,
            &self.inline_stack,
            self.heading_style,
        );
        match event {
            Event::Start(tag) => self.handle_start(tag),
            Event::End(tag_end) => self.handle_end(tag_end),
            Event::Text(content) => self.handle_text(&content, span_style),
            Event::Code(content) => self.handle_code(content.into_string(), span_style),
            Event::Html(content) | Event::InlineHtml(content) => {
                append_link_text(&mut self.current_link, &content);
                self.push_text_or_cell(&content, span_style);
            }
            Event::InlineMath(content) | Event::DisplayMath(content) => {
                let style = span_style.patch(Style::default().fg(Color::Magenta));
                self.push_text_or_cell(&content, style);
            }
            Event::TaskListMarker(checked) => self.handle_task_marker(checked, span_style),
            Event::SoftBreak | Event::HardBreak => self.handle_break(span_style),
            Event::Rule => self.handle_rule(),
            Event::FootnoteReference(content) => {
                let style = span_style.patch(Style::default().add_modifier(Modifier::UNDERLINED));
                push_inline_or_cell(
                    &mut self.table_state,
                    &mut self.emitter.current_spans,
                    format!("[{content}]"),
                    style,
                );
            }
        }
    }

    fn handle_text(&mut self, content: &str, span_style: Style) {
        append_link_text(&mut self.current_link, content);
        if self.in_code_block {
            self.code_block_buffer.push_str(content);
        } else {
            self.push_text_or_cell(content, span_style);
        }
    }

    fn handle_code(&mut self, content: String, span_style: Style) {
        append_link_text(&mut self.current_link, &content);
        let style = span_style.patch(Style::default().fg(Color::Cyan));
        push_inline_or_cell(
            &mut self.table_state,
            &mut self.emitter.current_spans,
            content,
            style,
        );
    }

    fn push_text_or_cell(&mut self, content: &str, style: Style) {
        if let Some(state) = self.table_state.as_mut() {
            state.push_text(content, style);
        } else {
            self.emitter
                .append_inline_text(&mut self.tracker, content, style);
        }
    }

    fn handle_task_marker(&mut self, checked: bool, span_style: Style) {
        let marker = if checked { "[x] " } else { "[ ] " };
        if let Some(state) = self.table_state.as_mut() {
            state.push_inline_span(marker.to_string(), span_style);
        } else if !self.emitter.current_spans.is_empty()
            || !tracker_try_attach_task_marker(&mut self.tracker, marker, self.emitter.base_style)
        {
            self.emitter
                .current_spans
                .push(Span::styled(marker.to_string(), span_style));
        }
    }

    fn handle_break(&mut self, span_style: Style) {
        if let Some(state) = self.table_state.as_mut() {
            state.push_newline(span_style);
        } else {
            self.emitter.flush_current_line(&mut self.tracker);
        }
    }

    fn handle_rule(&mut self) {
        self.begin_block(BlockKind::Rule);
        self.emitter.emit_prefixed_spans(
            &mut self.tracker,
            vec![Span::styled("────".to_string(), self.emitter.base_style)],
        );
        tracker_record_emitted(&mut self.tracker, BlockKind::Rule);
        if tracker_is_top_level(&self.tracker) {
            self.emitter.flush_paragraph_block();
        }
    }

    fn handle_start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.begin_block(BlockKind::Paragraph);
                self.emitter.emitted_in_current_leaf = false;
            }
            Tag::Heading { level, .. } => {
                self.begin_block(BlockKind::Heading);
                self.emitter.emitted_in_current_leaf = false;
                self.heading_style = Some(heading_style_for(level));
            }
            Tag::HtmlBlock => {
                self.begin_block(BlockKind::HtmlBlock);
                self.emitter.emitted_in_current_leaf = false;
            }
            Tag::BlockQuote(_) => {
                self.begin_block(BlockKind::BlockQuote);
                self.tracker.container_stack.push(Container::BlockQuote {
                    has_emitted_block: false,
                    last_block_kind: None,
                });
            }
            Tag::List(start) => {
                self.begin_block(BlockKind::List);
                self.tracker
                    .container_stack
                    .push(Container::List(ListState {
                        next_index: start.unwrap_or(1) as usize,
                        ordered: start.is_some(),
                        emitted_any: false,
                    }));
            }
            Tag::Item => tracker_push_item(&mut self.tracker, self.emitter.base_style),
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough => {
                push_inline(&mut self.inline_stack, inline_style_for_tag(&tag), false);
            }
            Tag::Link { dest_url, .. } => {
                self.current_link = Some((String::new(), dest_url.into_string()));
                push_inline(&mut self.inline_stack, link_style(), true);
            }
            Tag::CodeBlock(kind) => self.start_code_block(kind),
            Tag::Table(_) => {
                self.begin_block(BlockKind::Table);
                self.table_state = Some(TableState::default());
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {
                if let Some(state) = self.table_state.as_mut() {
                    state.handle_start(&tag);
                }
            }
            _ => {}
        }
    }

    fn start_code_block(&mut self, kind: CodeBlockKind<'_>) {
        self.begin_block(BlockKind::CodeBlock);
        self.in_code_block = true;
        self.code_block_lang = match kind {
            CodeBlockKind::Fenced(lang) => Some(lang.into_string()),
            CodeBlockKind::Indented => None,
        };
        self.code_block_buffer.clear();
    }

    fn handle_end(&mut self, tag_end: TagEnd) {
        match tag_end {
            TagEnd::Paragraph => self.finish_leaf(BlockKind::Paragraph),
            TagEnd::Heading(_) => {
                self.heading_style = None;
                self.finish_leaf(BlockKind::Heading);
            }
            TagEnd::HtmlBlock => self.finish_leaf(BlockKind::HtmlBlock),
            TagEnd::BlockQuote(_) => {
                self.emitter.flush_current_line(&mut self.tracker);
                let emitted = matches!(
                    self.tracker.container_stack.pop(),
                    Some(Container::BlockQuote {
                        has_emitted_block: true,
                        ..
                    })
                );
                self.finish_container_end(emitted, BlockKind::BlockQuote);
            }
            TagEnd::List(_) => {
                let emitted = matches!(
                    self.tracker.container_stack.pop(),
                    Some(Container::List(list)) if list.emitted_any
                );
                self.finish_container_end(emitted, BlockKind::List);
            }
            TagEnd::Item => self.finish_item_end(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                pop_inline(&mut self.inline_stack);
            }
            TagEnd::Link => self.finish_link_end(),
            TagEnd::CodeBlock => self.flush_code_block(),
            TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell | TagEnd::Table => {
                self.finish_table_end(tag_end);
            }
            _ => {}
        }
    }

    fn finish_item_end(&mut self) {
        self.emitter.flush_current_line(&mut self.tracker);
        let emit_empty = self
            .emitter
            .emit_empty_item_marker_if_pending(&mut self.tracker);
        let popped = self.tracker.container_stack.pop();
        tracker_record_popped_item(&mut self.tracker.container_stack, popped, emit_empty);
    }

    fn finish_link_end(&mut self) {
        if let Some((link_text, url)) = self.current_link.take() {
            self.links.push(ExtractedLink {
                text: link_text,
                url,
            });
        }
        pop_inline(&mut self.inline_stack);
    }

    fn finish_table_end(&mut self, tag_end: TagEnd) {
        let finished = self
            .table_state
            .as_mut()
            .is_some_and(|state| state.handle_end(tag_end));
        if finished {
            if let Some(state) = self.table_state.take() {
                self.emitter.materialize_pending_separator();
                self.emitter.flush_current_line(&mut self.tracker);
                self.emitter.flush_paragraph_block();
                self.emitter
                    .blocks
                    .push(build_table_block(state, self.emitter.width));
                tracker_record_emitted(&mut self.tracker, BlockKind::Table);
            }
        }
    }

    fn finish_leaf(&mut self, kind: BlockKind) {
        self.emitter.flush_current_line(&mut self.tracker);
        self.finish_container_end(self.emitter.emitted_in_current_leaf, kind);
    }

    fn finish_container_end(&mut self, emitted: bool, kind: BlockKind) {
        if emitted {
            tracker_record_emitted(&mut self.tracker, kind);
            if tracker_is_top_level(&self.tracker) {
                self.emitter.flush_paragraph_block();
            }
        } else {
            self.emitter
                .clear_pending_separator_at(self.tracker.container_stack.len());
        }
    }

    fn finish(mut self) -> RenderedEntry {
        self.emitter.finish(&mut self.tracker, self.links)
    }
}

fn inline_style_for_tag(tag: &Tag<'_>) -> Style {
    match tag {
        Tag::Emphasis => Style::default().add_modifier(Modifier::ITALIC),
        Tag::Strong => Style::default().add_modifier(Modifier::BOLD),
        Tag::Strikethrough => Style::default().add_modifier(Modifier::CROSSED_OUT),
        _ => Style::default(),
    }
}

fn link_style() -> Style {
    Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::UNDERLINED)
}

fn append_link_text(current_link: &mut Option<(String, String)>, content: &str) {
    if let Some((link_text, _)) = current_link.as_mut() {
        link_text.push_str(content);
    }
}

fn push_inline_or_cell(
    table_state: &mut Option<TableState>,
    current_spans: &mut Vec<Span<'static>>,
    content: String,
    style: Style,
) {
    if let Some(state) = table_state.as_mut() {
        state.push_inline_span(content, style);
    } else {
        current_spans.push(Span::styled(content, style));
    }
}

fn current_segment_list_depth(container_stack: &[Container]) -> usize {
    container_stack
        .iter()
        .rev()
        .take_while(|c| !matches!(c, Container::BlockQuote { .. }))
        .filter(|c| matches!(c, Container::ListItem(_)))
        .count()
}

fn separator_line(
    container_stack: &[Container],
    bq_depth: usize,
    base_style: Style,
    width: u16,
) -> Line<'static> {
    if bq_depth == 0 {
        return Line::default();
    }
    let mut atoms = separator_prefix_atoms(container_stack, bq_depth);
    if width > 0 {
        clamp_atoms_total(&mut atoms, max_prefix_width(width));
    }
    Line::from(atoms_to_spans(&atoms, base_style))
}

fn next_non_list_is_item(container_stack: &[Container], idx: usize) -> bool {
    matches!(
        container_stack[idx + 1..]
            .iter()
            .find(|c| !matches!(c, Container::List(_))),
        Some(Container::ListItem(_))
    )
}

fn separator_prefix_atoms(container_stack: &[Container], bq_depth: usize) -> Vec<PrefixAtom> {
    let mut atoms = Vec::new();
    let mut seen_bq = 0usize;
    for (i, container) in container_stack.iter().enumerate() {
        let next_is_item = next_non_list_is_item(container_stack, i);
        match container {
            Container::BlockQuote { .. } => {
                atoms.push(PrefixAtom::Bar);
                seen_bq += 1;
                if seen_bq >= bq_depth {
                    break;
                }
            }
            Container::ListItem(item) if !next_is_item => {
                let indent = item.nest_spaces + item.content_offset;
                if indent > 0 {
                    atoms.push(PrefixAtom::Space(indent));
                }
            }
            Container::ListItem(_) | Container::List(_) => {}
        }
    }
    atoms
}

fn build_line_prefixes(
    container_stack: &mut [Container],
    base_style: Style,
    width: u16,
    consume_marker: bool,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let (mut first_atoms, mut cont_atoms) = collect_prefix_atoms(container_stack, consume_marker);
    clamp_prefix_pair(&mut first_atoms, &mut cont_atoms, width);
    (
        atoms_to_spans(&first_atoms, base_style),
        atoms_to_spans(&cont_atoms, base_style),
    )
}

fn collect_prefix_atoms(
    container_stack: &mut [Container],
    consume_marker: bool,
) -> (Vec<PrefixAtom>, Vec<PrefixAtom>) {
    let mut first_atoms = Vec::new();
    let mut cont_atoms = Vec::new();
    let len = container_stack.len();

    for i in 0..len {
        let next_is_item = next_non_list_is_item(container_stack, i);
        match &mut container_stack[i] {
            Container::BlockQuote { .. } => {
                first_atoms.push(PrefixAtom::Bar);
                cont_atoms.push(PrefixAtom::Bar);
            }
            Container::ListItem(item) if !next_is_item => {
                append_item_atoms(item, consume_marker, &mut first_atoms, &mut cont_atoms);
            }
            Container::ListItem(_) | Container::List(_) => {}
        }
    }
    (first_atoms, cont_atoms)
}

fn markdown_parser_options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES
}

pub fn render_markdown(
    text: &str,
    base_style: Style,
    width: u16,
    theme: Option<&Theme>,
) -> RenderedEntry {
    let parser = Parser::new_ext(text, markdown_parser_options());
    let mut renderer = MarkdownRenderer::new(base_style, width, theme);

    for event in parser {
        renderer.handle_event(event);
    }

    renderer.finish()
}

/// Extracts unique links from markdown text, deduplicated by URL (preserving first appearance).
pub fn extract_markdown_links(markdown: &str) -> Vec<ExtractedLink> {
    let rendered = render_markdown(markdown, Style::default(), 80, None);
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for link in rendered.links {
        if link.url.trim().is_empty() {
            continue;
        }
        if seen.insert(link.url.clone()) {
            unique.push(link);
        }
    }
    unique
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

fn next_list_prefix(list: &mut ListState, base_style: Style) -> (String, Style) {
    if list.ordered {
        let prefix = format!("{}. ", list.next_index);
        list.next_index += 1;
        (prefix, base_style)
    } else {
        ("- ".to_string(), base_style)
    }
}

fn split_code_lines(raw: &str) -> Vec<&str> {
    let body = if let Some(stripped) = raw.strip_suffix("\r\n") {
        stripped
    } else if let Some(stripped) = raw.strip_suffix('\n') {
        stripped
    } else {
        raw
    };
    body.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn resolve_code_syntax<'s>(
    syntax_set: &'s SyntaxSet,
    lang: Option<&str>,
    code_lines: &[&str],
) -> Option<&'s SyntaxReference> {
    if let Some(syntax) = lang.and_then(|l| find_syntax_by_name(syntax_set, l)) {
        return Some(syntax);
    }
    let first_non_empty = code_lines
        .iter()
        .copied()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    if first_non_empty.starts_with("diff --git") || lang == Some("diff") {
        find_syntax_by_name(syntax_set, "diff")
    } else {
        find_syntax_by_first_line(syntax_set, first_non_empty)
    }
}

fn highlight_code_lines(
    code_lines: &[&str],
    lang: Option<&str>,
    base_style: Style,
    theme: Option<&Theme>,
) -> Vec<Vec<Span<'static>>> {
    if let (Some(syntax_set), Some(theme)) = (SYNTAX_SET.as_ref(), theme) {
        let syntax = resolve_code_syntax(syntax_set, lang, code_lines);
        let mut highlighter = syntax.map(|s| HighlightLines::new(s, theme));
        return code_lines
            .iter()
            .map(|line| {
                highlight_single_code_line(line, highlighter.as_mut(), syntax_set, base_style)
            })
            .collect();
    }

    let fallback_style = base_style.patch(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    );
    code_lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                Vec::new()
            } else {
                vec![Span::styled((*line).to_string(), fallback_style)]
            }
        })
        .collect()
}

fn highlight_single_code_line(
    line: &str,
    highlighter: Option<&mut HighlightLines>,
    syntax_set: &SyntaxSet,
    base_style: Style,
) -> Vec<Span<'static>> {
    match highlighter {
        Some(hl) => highlight_line_to_spans(line, hl, syntax_set, base_style),
        None => non_empty_plain_spans(line, base_style),
    }
}

fn non_empty_plain_spans(line: &str, base_style: Style) -> Vec<Span<'static>> {
    if line.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(line.to_string(), base_style)]
    }
}

fn max_prefix_width(width: u16) -> usize {
    let min_content = (width as usize / 2).clamp(1, MIN_WRAPPED_CONTENT_WIDTH);
    (width as usize).saturating_sub(min_content)
}

fn append_item_atoms(
    item: &mut ListItemContext,
    consume_marker: bool,
    first_atoms: &mut Vec<PrefixAtom>,
    cont_atoms: &mut Vec<PrefixAtom>,
) {
    if item.nest_spaces > 0 {
        first_atoms.push(PrefixAtom::Space(item.nest_spaces));
        cont_atoms.push(PrefixAtom::Space(item.nest_spaces));
    }
    let marker = if consume_marker {
        item.pending_marker.take()
    } else {
        item.pending_marker.clone()
    };
    if let Some((text, style)) = marker {
        first_atoms.push(PrefixAtom::Marker(text, style));
    } else if item.content_offset > 0 {
        first_atoms.push(PrefixAtom::Space(item.content_offset));
    }
    if item.content_offset > 0 {
        cont_atoms.push(PrefixAtom::Space(item.content_offset));
    }
}

fn clamp_prefix_pair(
    first_atoms: &mut Vec<PrefixAtom>,
    cont_atoms: &mut Vec<PrefixAtom>,
    width: u16,
) {
    if width == 0 {
        return;
    }
    let first_w = atoms_width(first_atoms);
    let cont_w = atoms_width(cont_atoms);
    if first_w < width as usize && cont_w < width as usize {
        return;
    }
    let max_prefix = max_prefix_width(width);
    clamp_atoms_preserving_marker(first_atoms, max_prefix);
    let first_clamped_w = atoms_width(first_atoms);
    let target_cont = max_prefix.min(first_clamped_w.max(cont_w.min(max_prefix)));
    clamp_atoms_total(cont_atoms, target_cont);
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

fn atoms_width(atoms: &[PrefixAtom]) -> usize {
    atoms
        .iter()
        .map(|atom| match atom {
            PrefixAtom::Bar => 2,
            PrefixAtom::Space(n) => *n,
            PrefixAtom::Marker(text, _) => UnicodeWidthStr::width(text.as_str()),
        })
        .sum()
}

fn clamp_atoms_preserving_marker(atoms: &mut Vec<PrefixAtom>, max_width: usize) {
    let mut current = atoms_width(atoms);
    if current <= max_width {
        return;
    }
    // First shrink Space atoms from innermost to outermost.
    for atom in atoms.iter_mut().rev() {
        if current <= max_width {
            break;
        }
        if let PrefixAtom::Space(n) = atom {
            let reduce = (current - max_width).min(*n);
            *n -= reduce;
            current -= reduce;
        }
    }
    atoms.retain(|atom| !matches!(atom, PrefixAtom::Space(0)));
}

fn clamp_atoms_total(atoms: &mut Vec<PrefixAtom>, max_width: usize) {
    clamp_atoms_preserving_marker(atoms, max_width);
    let mut current = atoms_width(atoms);
    if current <= max_width {
        return;
    }
    // If bars/markers still exceed max_width, drop from rightmost.
    while current > max_width {
        match atoms.pop() {
            Some(PrefixAtom::Bar) => current = current.saturating_sub(2),
            Some(PrefixAtom::Space(n)) => current = current.saturating_sub(n),
            Some(PrefixAtom::Marker(text, _)) => {
                current = current.saturating_sub(UnicodeWidthStr::width(text.as_str()));
            }
            None => break,
        }
    }
}

fn atoms_to_spans(atoms: &[PrefixAtom], base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for atom in atoms {
        match atom {
            PrefixAtom::Bar => {
                spans.push(Span::styled("│ ", Style::default().fg(Color::DarkGray)));
            }
            PrefixAtom::Space(n) if *n > 0 => {
                spans.push(Span::styled(" ".repeat(*n), base_style));
            }
            PrefixAtom::Space(_) => {}
            PrefixAtom::Marker(text, style) => {
                spans.push(Span::styled(text.clone(), *style));
            }
        }
    }
    spans
}

fn first_prefix_has_marker(spans: &[Span<'_>]) -> bool {
    spans.iter().any(|s| {
        let t = s.content.as_ref();
        !t.is_empty() && !t.chars().all(|c| c == ' ') && t != "│ "
    })
}

fn wrap_prefixed_spans(
    first_prefix: Vec<Span<'static>>,
    cont_prefix: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    width: u16,
) -> Vec<Line<'static>> {
    if width == 0 {
        let mut spans = first_prefix;
        spans.extend(content);
        return vec![Line::from(spans)];
    }

    if content.is_empty() {
        return vec![Line::from(first_prefix)];
    }

    let first_w = spans_width(&first_prefix);
    let cont_w = spans_width(&cont_prefix);
    let avail = width
        .saturating_sub(cont_w.min(u16::MAX as usize) as u16)
        .max(1);
    let wrapped = wrap_spans(content, avail);

    if wrapped.is_empty() {
        return vec![Line::from(first_prefix)];
    }

    let first_line_fits = first_w < width as usize
        && (first_w <= cont_w || first_w + spans_width(&wrapped[0].spans) <= width as usize);

    let mut lines = Vec::with_capacity(wrapped.len() + usize::from(!first_line_fits));
    if first_line_fits {
        for (idx, line) in wrapped.into_iter().enumerate() {
            let mut spans = if idx == 0 {
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    } else {
        lines.push(Line::from(first_prefix));
        for line in wrapped {
            let mut spans = cont_prefix.clone();
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }
    lines
}

fn take_code_line_spans(
    graphemes: &[(String, Style, usize)],
    start: usize,
    budget: usize,
) -> (Vec<Span<'static>>, usize) {
    let mut idx = start;
    let mut used_width = 0usize;
    let mut spans: Vec<Span<'static>> = Vec::new();
    while let Some((sym, style, gw)) = graphemes.get(idx) {
        if idx > start && used_width + *gw > budget {
            break;
        }
        used_width += *gw;
        if let Some(last) = spans.last_mut().filter(|s| s.style == *style) {
            last.content.to_mut().push_str(sym);
        } else {
            spans.push(Span::styled(sym.clone(), *style));
        }
        idx += 1;
    }
    (spans, idx)
}

/// Width-budget grapheme wrapping for literal code rows (fenced code, code
/// inside list items/blockquotes). Unlike the prose wrapper `wrap_spans`
/// (textwrap), this keeps leading, interior, and trailing spaces verbatim —
/// textwrap collapses whitespace, which corrupts literal code content
/// (#2934 review regression). Code rows must never go through the prose
/// wrapper, and wrapped code rows are distinct from structural blank
/// separators.
fn wrap_prefixed_code_spans(
    first_prefix: Vec<Span<'static>>,
    cont_prefix: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    width: u16,
) -> Vec<Line<'static>> {
    if width == 0 || content.is_empty() {
        let mut spans = first_prefix;
        spans.extend(content);
        return vec![Line::from(spans)];
    }

    let graphemes: Vec<(String, Style, usize)> = content
        .iter()
        .flat_map(|span| {
            span.styled_graphemes(Style::default()).map(|g| {
                let w = UnicodeWidthStr::width(g.symbol);
                (g.symbol.to_string(), g.style, w)
            })
        })
        .collect();
    if graphemes.is_empty() {
        return vec![Line::from(first_prefix)];
    }

    let max_width = width as usize;
    let first_w = spans_width(&first_prefix);
    let cont_w = spans_width(&cont_prefix);
    let first_avail = max_width.saturating_sub(first_w);
    let cont_avail = max_width.saturating_sub(cont_w).max(1);

    let defer_first_line = first_avail == 0
        || (first_w > cont_w && graphemes[0].2 > first_avail && graphemes[0].2 <= cont_avail);

    let mut lines = Vec::new();
    let mut idx = 0usize;
    if defer_first_line {
        lines.push(Line::from(first_prefix));
    } else {
        let (first_spans, next_idx) = take_code_line_spans(&graphemes, 0, first_avail);
        let mut spans = first_prefix;
        spans.extend(first_spans);
        lines.push(Line::from(spans));
        idx = next_idx;
    }

    while idx < graphemes.len() {
        let (cont_spans, next_idx) = take_code_line_spans(&graphemes, idx, cont_avail);
        let mut spans = cont_prefix.clone();
        spans.extend(cont_spans);
        lines.push(Line::from(spans));
        idx = next_idx;
    }
    lines
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
    if line.is_empty() {
        let _ = highlighter.highlight_line("", syntax_set);
        return Vec::new();
    }
    match highlighter.highlight_line(line, syntax_set) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(style, text)| {
                Span::styled(
                    text.to_string(),
                    syntect_style_to_ratatui(style, base_style),
                )
            })
            .collect(),
        Err(_) => vec![Span::styled(line.to_string(), base_style)],
    }
}

fn syntect_style_to_ratatui(style: syntect::highlighting::Style, base_style: Style) -> Style {
    let mut ratatui_style = Style::default();
    if let Some(fg) = convert_syntect_color(style.foreground) {
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
    base_style.patch(ratatui_style)
}

/// Convert a syntect Color to a ratatui Color (RGB).
fn convert_syntect_color(color: SyntectColor) -> Option<Color> {
    // Check for fully transparent (syntect default for unspecified)
    if color.a == 0 {
        return None;
    }
    Some(Color::Rgb(color.r, color.g, color.b))
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
        extract_markdown_links, render_markdown, shrink_table_widths, wrap_spans, ExtractedLink,
        MarkdownBlockData, RenderedEntry,
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

        syntect::dumps::from_uncompressed_data::<Theme>(bytes).expect("decode builtin theme")
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

    #[test]
    fn extract_links_basic_and_autolinks() {
        let md =
            "Check out [Rust](https://www.rust-lang.org) and <https://crates.io> for packages.";
        let links = extract_markdown_links(md);
        assert_eq!(
            links,
            vec![
                ExtractedLink {
                    text: "Rust".to_string(),
                    url: "https://www.rust-lang.org".to_string(),
                },
                ExtractedLink {
                    text: "https://crates.io".to_string(),
                    url: "https://crates.io".to_string(),
                },
            ]
        );
    }

    #[test]
    fn extract_links_with_inline_formatting_and_code() {
        let md = "See [**bold** and `code`](https://example.com/formatted).";
        let links = extract_markdown_links(md);
        assert_eq!(
            links,
            vec![ExtractedLink {
                text: "bold and code".to_string(),
                url: "https://example.com/formatted".to_string(),
            }]
        );
    }

    #[test]
    fn extract_links_deduplicates_by_url() {
        let md = "Link [first](https://example.com) and [second](https://example.com) and [other](https://other.com).";
        let links = extract_markdown_links(md);
        assert_eq!(
            links,
            vec![
                ExtractedLink {
                    text: "first".to_string(),
                    url: "https://example.com".to_string(),
                },
                ExtractedLink {
                    text: "other".to_string(),
                    url: "https://other.com".to_string(),
                },
            ]
        );
    }

    #[test]
    fn extract_links_empty_and_no_links() {
        assert!(extract_markdown_links("").is_empty());
        assert!(extract_markdown_links("Plain text without any link.").is_empty());
    }

    #[test]
    fn extract_links_in_table() {
        let md = "| Name | Site |\n| --- | --- |\n| Google | [Search](https://google.com) |";
        let links = extract_markdown_links(md);
        assert_eq!(
            links,
            vec![ExtractedLink {
                text: "Search".to_string(),
                url: "https://google.com".to_string(),
            }]
        );
    }

    #[test]
    fn rendered_entry_contains_links() {
        let rendered = render_markdown(
            "Read [Harnx](https://harnx.ai) for details.",
            Style::default(),
            80,
            None,
        );
        assert_eq!(
            rendered.links,
            vec![ExtractedLink {
                text: "Harnx".to_string(),
                url: "https://harnx.ai".to_string(),
            }]
        );
    }

    fn sum_block_heights(entry: &RenderedEntry) -> u16 {
        entry
            .blocks
            .iter()
            .map(|block| match block {
                MarkdownBlockData::Paragraph { height, .. } => *height,
                MarkdownBlockData::Table { height, .. } => *height,
            })
            .sum()
    }

    fn paint_rows(
        markdown: &str,
        width: u16,
        theme: Option<&Theme>,
    ) -> (RenderedEntry, Vec<String>) {
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

        let entry = render_markdown(markdown, Style::default(), width, theme);
        assert_eq!(entry.total_height, sum_block_heights(&entry));
        let mut buf = Buffer::empty(Rect::new(0, 0, width, entry.total_height));
        entry.clone().render(buf.area, &mut buf);
        let rows = (0..entry.total_height)
            .map(|y| {
                let line: String = (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>();
                line.trim_end().to_string()
            })
            .collect();
        (entry, rows)
    }

    fn paint_raw_rows(
        markdown: &str,
        width: u16,
        theme: Option<&Theme>,
    ) -> (RenderedEntry, Vec<String>, ratatui::buffer::Buffer) {
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

        let entry = render_markdown(markdown, Style::default(), width, theme);
        assert_eq!(entry.total_height, sum_block_heights(&entry));
        let mut buf = Buffer::empty(Rect::new(0, 0, width, entry.total_height));
        entry.clone().render(buf.area, &mut buf);
        let rows = (0..entry.total_height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect();
        (entry, rows, buf)
    }

    #[test]
    fn top_level_block_boundaries_have_exactly_one_separator_and_no_outer_padding() {
        let md = "# Heading\n\nIntro paragraph.\n\n- one\n- two\n\n> quoted\n\n```txt\ncode\n```\n\n| A |\n| - |\n| 1 |\n\n---\n\nOutro.";
        let (_entry, rows) = paint_rows(md, 40, None);
        assert_eq!(
            rows,
            vec![
                "Heading",
                "",
                "Intro paragraph.",
                "",
                "- one",
                "- two",
                "",
                "│ quoted",
                "",
                "code",
                "",
                "┌──────────────────────────────────────┐",
                "│A                                     │",
                "│1                                     │",
                "│                                      │",
                "└──────────────────────────────────────┘",
                "",
                "────",
                "",
                "Outro.",
            ]
        );
    }

    #[test]
    fn empty_and_single_line_inputs_keep_compact_shape() {
        let (empty, empty_rows) = paint_rows("", 40, None);
        assert!(empty.blocks.is_empty());
        assert_eq!(empty.total_height, 0);
        assert!(empty_rows.is_empty());

        let (single, single_rows) = paint_rows("single compact line", 40, None);
        assert_eq!(single.blocks.len(), 1);
        assert_eq!(single.total_height, 1);
        assert_eq!(single_rows, vec!["single compact line"]);
    }

    #[test]
    fn empty_blocks_do_not_double_separators() {
        let md = "First\n\n```rust\n```\n\n#\n\nSecond";
        let (_entry, rows) = paint_rows(md, 40, None);
        assert_eq!(rows, vec!["First", "", "Second"]);
    }

    #[test]
    fn tight_and_single_para_loose_lists_stay_compact_while_multi_para_loose_separates() {
        let tight = "- a\n- b\n- c";
        let (_, tight_rows) = paint_rows(tight, 40, None);
        assert_eq!(tight_rows, vec!["- a", "- b", "- c"]);

        let loose_single = "- a\n\n- b";
        let (_, loose_single_rows) = paint_rows(loose_single, 40, None);
        assert_eq!(loose_single_rows, vec!["- a", "- b"]);

        let loose_multi = "- first para\n\n  second para\n\n- next item";
        let (_, loose_multi_rows) = paint_rows(loose_multi, 40, None);
        assert_eq!(
            loose_multi_rows,
            vec!["- first para", "", "  second para", "- next item"]
        );
    }

    #[test]
    fn nested_lists_preserve_two_space_hierarchy_and_ordered_starts() {
        let md = "5. fifth\n   - child a\n     10. grand\n6. sixth\n\n-\n  - direct";
        let (_, rows) = paint_rows(md, 40, None);
        assert_eq!(
            rows,
            vec![
                "5. fifth",
                "  - child a",
                "    10. grand",
                "6. sixth",
                "",
                "-",
                "  - direct",
            ]
        );
    }

    #[test]
    fn wrapped_lists_and_task_lists_use_hanging_indentation() {
        let md = "- alpha beta gamma\n10. one two three\n- [ ] todo item wraps\n  - [x] nested done wraps";
        let (_, rows) = paint_rows(md, 16, None);
        assert_eq!(
            rows,
            vec![
                "- alpha beta",
                "  gamma",
                "10. one two",
                "    three",
                "- [ ] todo item",
                "      wraps",
                "  - [x] nested",
                "        done",
                "        wraps",
            ]
        );
    }

    #[test]
    fn blockquote_list_and_nested_blockquote_separators_align_without_multiplying() {
        let md = "> intro\n>\n> - alpha beta gamma\n>\n> outro\n\n- outer\n\n  > q1\n  >\n  > q2";
        let (_, rows) = paint_rows(md, 18, None);
        assert_eq!(
            rows,
            vec![
                "│ intro",
                "│",
                "│ - alpha beta",
                "│   gamma",
                "│",
                "│ outro",
                "",
                "- outer",
                "",
                "  │ q1",
                "  │",
                "  │ q2",
            ]
        );
    }

    #[test]
    fn wrapped_list_preserves_inline_styles_and_link_order() {
        let md = "- first **bold** [link1](https://one.test) and `code` [link2](https://two.test)";
        let (entry, rows) = paint_rows(md, 24, None);
        assert_eq!(
            entry.links,
            vec![
                ExtractedLink {
                    text: "link1".to_string(),
                    url: "https://one.test".to_string(),
                },
                ExtractedLink {
                    text: "link2".to_string(),
                    url: "https://two.test".to_string(),
                },
            ]
        );
        assert!(rows.len() > 1);
        assert!(rows[1].starts_with("  "));
        let bold = find_span(&entry, "bold");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = find_span(&entry, "code");
        assert_eq!(code.style.fg, Some(Color::Cyan));
        let link1 = find_span(&entry, "link1");
        assert_eq!(link1.style.fg, Some(Color::Blue));
        assert!(link1.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn fenced_code_blank_rows_paint_with_and_without_theme_and_boundaries() {
        let dark = load_builtin_theme("monokai-extended");
        let md = "# Title\n\n```rust\nlet a = 1;\n\nlet b = 2;\n```\n\nAfter.";
        for theme in [None, Some(&dark)] {
            let (entry, rows) = paint_rows(md, 40, theme);
            assert_eq!(entry.total_height, 7);
            assert_eq!(
                rows,
                vec!["Title", "", "let a = 1;", "", "let b = 2;", "", "After.",]
            );
        }

        let (_, single_blank_rows) = paint_rows("```\n\n```", 20, Some(&dark));
        assert_eq!(single_blank_rows, vec![""]);

        let (_, quoted_code_rows) = paint_rows("> ```\n> a\n>\n> b\n> ```", 20, None);
        assert_eq!(quoted_code_rows, vec!["│ a", "│", "│ b"]);

        let (_, list_code_rows) = paint_rows("- ```\n  a\n\n  b\n  ```", 20, None);
        assert_eq!(list_code_rows, vec!["- a", "", "  b"]);
    }

    #[test]
    fn narrow_and_zero_widths_are_safe_and_preserve_height_parity() {
        let md = "# Heading\n\n100. **item** text wraps\n  - [x] sub task\n\n> quoted\n\n| A | B |\n| - | - |\n| x | y |";
        let zero = render_markdown(md, Style::default(), 0, None);
        assert_eq!(zero.total_height, sum_block_heights(&zero));
        assert!(!zero.blocks.is_empty());

        for width in [1u16, 2, 3, 5, 10, 12, 20, 40, 80] {
            let (entry, rows) = paint_rows(md, width, None);
            assert_eq!(rows.len() as u16, entry.total_height, "width={width}");
            let compact_chars: String = rows
                .concat()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            assert!(
                compact_chars.contains("item"),
                "content disappeared at width={width}: {rows:?}"
            );
        }
    }

    #[test]
    fn overflowing_container_code_preserves_literal_whitespace_and_styles() {
        let dark = load_builtin_theme("monokai-extended");
        let quote_end_spaces = "> ```\n> 1234567890123456789012        b\n> ```";
        let quote_cross_spaces = "> ```\n> 12345678901234567890        b\n> ```";
        let list_cross_spaces = "- ```\n  1234567890    b\n  ```";

        for theme in [None, Some(&dark)] {
            let (e1, r1, _) = paint_raw_rows(quote_end_spaces, 24, theme);
            assert_eq!(e1.total_height, 2);
            assert_eq!(
                r1,
                vec!["│ 1234567890123456789012", "│         b             ",]
            );

            let (e2, r2, _) = paint_raw_rows(quote_cross_spaces, 24, theme);
            assert_eq!(e2.total_height, 2);
            assert_eq!(
                r2,
                vec!["│ 12345678901234567890  ", "│       b               ",]
            );

            let (e3, r3, _) = paint_raw_rows(list_cross_spaces, 14, theme);
            assert_eq!(e3.total_height, 2);
            assert_eq!(r3, vec!["- 1234567890  ", "    b         "]);
        }

        // Verify syntect-themed multi-span wrapping preserves prefix and token colors.
        let themed_rust = "> ```rust\n> let alpha_value = \"hello_world\";\n> ```";
        let (e4, r4, buf4) = paint_raw_rows(themed_rust, 20, Some(&dark));
        assert_eq!(e4.total_height, r4.len() as u16);
        assert_eq!(r4[0], "│ let alpha_value = ");
        assert_eq!(r4[1], "│ \"hello_world\";    ");
        assert_eq!(buf4[(0, 0)].fg, Color::DarkGray);
        assert_eq!(buf4[(0, 1)].fg, Color::DarkGray);
        assert!(matches!(buf4[(2, 0)].fg, Color::Rgb(_, _, _)));
        assert!(matches!(buf4[(2, 1)].fg, Color::Rgb(_, _, _)));
    }

    #[test]
    fn overflowing_container_code_preserves_leading_indent_whitespace_rows_and_unicode() {
        let dark = load_builtin_theme("monokai-extended");
        // Indented overflowing token in a list item must keep leading spaces without a phantom blank row.
        let indented_list = "- ```\n      very_long_token\n  ```";
        // Ten literal spaces inside a quoted fence wrap into two prefixed rows.
        let whitespace_only_quote = "> ```\n>           \n> ```";

        for theme in [None, Some(&dark)] {
            let (e1, r1, _) = paint_raw_rows(indented_list, 12, theme);
            assert_eq!(e1.total_height, 2);
            assert_eq!(r1, vec!["-     very_l", "  ong_token "]);

            let (e2, r2, buf2) = paint_raw_rows(whitespace_only_quote, 8, theme);
            assert_eq!(e2.total_height, 2);
            assert_eq!(r2, vec!["│       ", "│       "]);
            if theme.is_none() {
                assert_eq!(buf2[(7, 0)].fg, Color::DarkGray);
                assert_eq!(buf2[(5, 1)].fg, Color::DarkGray);
                assert_eq!(buf2[(6, 1)].fg, Color::Reset);
            }
        }

        // Wide grapheme at wrap boundary stays intact and aligns on continuation line.
        let unicode_quote = "> ```\n> abcdefg💖h\n> ```";
        let (e3, r3, buf3) = paint_raw_rows(unicode_quote, 10, None);
        assert_eq!(e3.total_height, 2);
        assert_eq!(r3[0], "│ abcdefg ");
        assert_eq!(buf3[(2, 1)].symbol(), "💖");
        assert_eq!(buf3[(4, 1)].symbol(), "h");

        // Tiny width (avail=1 with 2-col grapheme) and multi-block separators + literal blank rows remain consistent.
        let tiny = render_markdown("> ```\n> 💖a\n> ```", Style::default(), 3, None);
        assert_eq!(tiny.total_height, sum_block_heights(&tiny));

        let multi = "> intro\n>\n> ```\n> 123456\n>\n> abcdef\n> ```\n>\n> outro";
        let (e4, r4, _) = paint_raw_rows(multi, 6, None);
        assert_eq!(e4.total_height, r4.len() as u16);
        assert_eq!(
            r4,
            vec![
                "│ intr", "│ o   ", "│     ", "│ 1234", "│ 56  ", "│     ", "│ abcd", "│ ef  ",
                "│     ", "│ outr", "│ o   ",
            ]
        );
    }
}
#[test]
fn test_wrap_panic_3() {
    let spans = vec![ratatui::text::Span::raw("💖   ")];
    let lines = wrap_spans(spans, 2);
    println!("{:?}", lines);
}
