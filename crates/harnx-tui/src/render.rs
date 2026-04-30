use crate::types::Tui;
use crate::types::{
    App, ToolCallBody, TranscriptItem, MAX_INPUT_HEIGHT, MIN_INPUT_HEIGHT, SPINNER_FRAMES,
};
use harnx_runtime::config::GlobalConfig;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

impl Tui {
    fn render_text_entry(
        prefix: &str,
        text: &str,
        style: Style,
        add_trailing_spacing: bool,
    ) -> Vec<Line<'static>> {
        let mut lines = vec![];
        for (index, line) in text.lines().enumerate() {
            if index == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), style),
                    Span::styled(line.to_string(), style),
                ]));
            } else {
                lines.push(Line::from(Span::styled(line.to_string(), style)));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(prefix.to_string(), style)));
        }
        if add_trailing_spacing {
            lines.push(Line::from(""));
        }
        lines
    }

    /// 3-space indent + a single line of inline-markdown body, used by
    /// templated tool result/call lines so `**bold**` / `` `code` `` add
    /// styling on top of the dim base without losing visual subordination.
    fn render_indented_markdown_line(text: &str) -> Line<'static> {
        let dim_gray = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        let body_base = Style::default().add_modifier(Modifier::DIM);
        let mut spans = vec![Span::styled("   ".to_string(), dim_gray)];
        let parsed = crate::render_helpers::markdown_line_spans(text, body_base);
        spans.extend(parsed.spans);
        Line::from(spans)
    }

    /// Render a `ToolCall` transcript item: `→ tool_name` header followed
    /// by the body lines. Body rendering depends on its origin —
    /// `Yaml` is shown verbatim (raw arguments), `Markdown` is rendered
    /// inline (templated `call_template` output).
    fn render_tool_call(tool_name: &str, body: Option<&ToolCallBody>) -> Vec<Line<'static>> {
        // Use the plain rightwards arrow `→` (U+2192). The previous glyph
        // was `->` followed by VS16 (U+FE0F) which requested an emoji-style
        // presentation and produced unicode-width vs. terminal-rendered-
        // width disagreement, leaving stray glyphs in subsequent frames.
        let dim_gray = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        let mut lines = Self::render_text_entry("", &format!("→ {tool_name}"), dim_gray, false);
        match body {
            Some(ToolCallBody::Yaml(yaml)) => {
                for line in yaml.lines() {
                    lines.extend(Self::render_text_entry("   ", line, dim_gray, false));
                }
            }
            Some(ToolCallBody::Markdown(md)) => {
                for line_text in md.lines() {
                    lines.push(Self::render_indented_markdown_line(line_text));
                }
            }
            None => {}
        }
        lines
    }

    pub(super) fn render_entry(entry: &TranscriptItem) -> Vec<Line<'static>> {
        match entry {
            TranscriptItem::SourceHeading(source) => Self::render_text_entry(
                "",
                &crate::render_helpers::source_heading(source),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                false,
            ),
            TranscriptItem::SystemText(text) => Self::render_text_entry(
                "",
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                false,
            ),
            TranscriptItem::UserText(text) => Self::render_text_entry(
                "> ",
                text,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                true,
            ),
            TranscriptItem::AssistantText(text) => {
                // Render assistant messages as markdown so headings, lists,
                // code fences, and inline emphasis show their styling.
                // Streaming chunks rebuild this entry on every render — an
                // unclosed `**bold` mid-stream simply renders as literal
                // asterisks for the moment, then upgrades to bold once the
                // closing `**` arrives in a later chunk.
                let mut lines = crate::render_helpers::markdown_lines(text, Style::default());
                // Match the prior trailing-spacing rule: pad after a
                // single-line message (so the next entry has breathing
                // room) but skip the pad when the text already contains
                // newlines.
                if !text.contains('\n') {
                    lines.push(Line::from(""));
                }
                lines
            }
            TranscriptItem::ErrorText(text) => Self::render_text_entry(
                "error: ",
                text,
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                true,
            ),
            TranscriptItem::ThoughtText(text) => Self::render_text_entry(
                "",
                &format!("<think>{text}</think>"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                false,
            ),
            TranscriptItem::ToolResultMarkdown(text) => {
                // Whole-document markdown path so block-level constructs
                // (fenced ```diff, lists, headings) parse correctly. Each
                // ratatui `Line` gets a 3-space indent prefix to keep the
                // visual subordination of tool result rows; the dim base
                // style is patched under each parsed span so plain text
                // still reads as dim.
                let dim_gray = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM);
                let body_base = Style::default().add_modifier(Modifier::DIM);
                crate::render_helpers::markdown_lines(text, body_base)
                    .into_iter()
                    .map(|line| {
                        let mut spans = vec![Span::styled("   ".to_string(), dim_gray)];
                        spans.extend(line.spans);
                        Line::from(spans)
                    })
                    .collect()
            }
            TranscriptItem::StatusLine(text) => Self::render_text_entry(
                "",
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                false,
            ),
            TranscriptItem::Plan(entries) => {
                let mut lines = Self::render_text_entry(
                    "",
                    "Plan:",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    false,
                );
                for entry in entries {
                    lines.extend(Self::render_text_entry(
                        "",
                        &format!("  [{}] {}", entry.status, entry.content),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                        false,
                    ));
                }
                lines
            }
            TranscriptItem::UsageLine(text) => Self::render_text_entry(
                "",
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                false,
            ),
            TranscriptItem::ToolCall { tool_name, body } => {
                Self::render_tool_call(tool_name, body.as_ref())
            }
            TranscriptItem::AttachmentHeader(text) => Self::render_text_entry(
                "",
                &format!("{text}:"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                false,
            ),
            TranscriptItem::AttachmentItem(text) => Self::render_text_entry(
                "  - ",
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                false,
            ),
            TranscriptItem::AttachmentPreviewLine(text) => Self::render_text_entry(
                "      ",
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                false,
            ),
        }
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame<'_>) {
        let size = frame.area();
        let input_width = size.width.saturating_sub(2).max(1);
        let input_height = self
            .input_height(input_width)
            .clamp(MIN_INPUT_HEIGHT, MAX_INPUT_HEIGHT);
        let attachment_height: u16 = if self.app.attachments.is_empty() {
            0
        } else {
            1
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(input_height + attachment_height),
            ])
            .split(size);

        let transcript_entries: Vec<Vec<Line<'static>>> = if self.app.transcript.is_empty() {
            vec![vec![Line::from(Span::raw(""))]]
        } else {
            self.app.transcript.iter().map(Self::render_entry).collect()
        };

        self.app
            .scroll_state
            .render(frame, chunks[0], &transcript_entries, |lines| {
                let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
                // Use Paragraph's own wrap-aware line count so the height we
                // report to the scroll widget exactly matches what the widget
                // will actually render.  Disagreement here causes the scroll
                // widget to allocate a mis-sized buffer, which in turn leaves
                // stale cells in the terminal and produces character-level
                // rendering artifacts (stray letters, corrupted words).
                let height = paragraph.line_count(chunks[0].width);
                (height, paragraph)
            });

        // Clamp position to the freshly-updated last_max_position.
        //
        // `scroll_down()` and `scroll_up()` operate against the *previous*
        // render's `last_max_position`.  When content grows between frames
        // (e.g. a streaming LLM response makes a transcript item taller),
        // the old ceiling is too small: `scroll_down` hits it prematurely and
        // sets `follow = true` at the wrong value.  On the next render the
        // real max is updated, but by then `position` is stuck above the
        // actual maximum.  Every subsequent `scroll_up` tick then burns off
        // the excess before any visual movement occurs — the "dead zone".
        //
        // Clamping here, immediately after the real max is known, prevents
        // position from ever drifting above `last_max_position`.  This costs
        // nothing (it is a simple saturating compare) and eliminates the
        // dead zone completely.
        if !self.app.scroll_state.follow {
            self.app.scroll_state.position = self
                .app
                .scroll_state
                .position
                .min(self.app.scroll_state.last_max_position);
        }

        self.app.last_known_input_width = chunks[1].width.saturating_sub(2).max(1);

        let title = self.build_input_title();
        self.app.input.set_block(
            Block::default()
                .borders(Borders::NONE)
                .title(title)
                .border_style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
        );
        frame.render_widget(&self.app.input, chunks[1]);

        if !self.app.attachments.is_empty() {
            let names: Vec<&str> = self
                .app
                .attachments
                .iter()
                .map(|a| a.display_name.as_str())
                .collect();
            let footer_text = format!("  Attached: {}   [.detach to remove]", names.join(", "));
            let footer_area = ratatui::layout::Rect::new(
                chunks[1].x,
                chunks[1].y + chunks[1].height - 1,
                chunks[1].width,
                1,
            );
            let footer = Paragraph::new(Line::from(Span::styled(
                footer_text,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
            )));
            frame.render_widget(footer, footer_area);
        }

        // Render completion popup above the input area
        if !self.app.completions.is_empty() {
            let max_visible = 8u16;
            let num_items = self.app.completions.len() as u16;
            let popup_height = num_items.min(max_visible) + 2; // +2 for border
            let popup_width = {
                let max_w = self
                    .app
                    .completions
                    .iter()
                    .map(|(v, d)| {
                        let desc_len = d.as_ref().map(|s| s.len() + 3).unwrap_or(0);
                        v.len() + desc_len
                    })
                    .max()
                    .unwrap_or(20);
                (max_w as u16 + 4).min(size.width.saturating_sub(4))
            };
            let popup_y = chunks[1].y.saturating_sub(popup_height);
            let popup_x = chunks[1].x + 1;
            let popup_area = ratatui::layout::Rect::new(
                popup_x,
                popup_y,
                popup_width.min(size.width.saturating_sub(popup_x)),
                popup_height,
            );

            let items: Vec<Line<'_>> = self
                .app
                .completions
                .iter()
                .enumerate()
                .map(|(i, (value, desc))| {
                    let is_selected = i == self.app.completion_index;
                    let style = if is_selected {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let mut spans = vec![Span::styled(value.clone(), style)];
                    if let Some(d) = desc {
                        spans.push(Span::styled(
                            format!("  {d}"),
                            if is_selected {
                                style.add_modifier(Modifier::DIM)
                            } else {
                                Style::default().add_modifier(Modifier::DIM)
                            },
                        ));
                    }
                    Line::from(spans)
                })
                .collect();

            let popup = Paragraph::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Completions")
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .scroll((
                    self.app
                        .completion_index
                        .saturating_sub(max_visible.saturating_sub(2) as usize)
                        as u16,
                    0,
                ));
            frame.render_widget(ratatui::widgets::Clear, popup_area);
            frame.render_widget(popup, popup_area);
        }
    }

    pub(super) fn input_height(&self, available_width: u16) -> u16 {
        let lines = self.app.input.lines();
        let body_width = available_width.max(1) as usize;

        let mut body_lines = 0usize;
        for line in lines {
            if line.is_empty() {
                body_lines = body_lines.saturating_add(1);
                continue;
            }
            let wrapped = textwrap::wrap(line, body_width).len().max(1);
            body_lines = body_lines.saturating_add(wrapped);
        }

        let total = body_lines
            .max(1)
            .min((u16::MAX as usize).saturating_sub(2))
            .saturating_add(2);
        total as u16
    }

    pub(super) fn append_streaming_assistant_chunk(&mut self, chunk: &str) {
        let mut remainder = chunk;
        while !remainder.is_empty() {
            if let Some(idx) = self.app.streaming_assistant_idx {
                match self.app.transcript.get_mut(idx) {
                    Some(TranscriptItem::AssistantText(existing)) => {
                        if existing.is_empty() {
                            if let Some(split_at) = remainder.find('\n') {
                                let (segment, rest) = remainder.split_at(split_at + 1);
                                existing.push_str(segment);
                                remainder = rest;
                                self.app.streaming_assistant_idx = None;
                            } else {
                                existing.push_str(remainder);
                                break;
                            }
                        } else if let Some(last_newline) = existing.rfind('\n') {
                            let tail = &existing[last_newline + 1..];
                            if tail.is_empty() {
                                self.app.streaming_assistant_idx = None;
                            } else if let Some(split_at) = remainder.find('\n') {
                                let (segment, rest) = remainder.split_at(split_at + 1);
                                existing.push_str(segment);
                                remainder = rest;
                                self.app.streaming_assistant_idx = None;
                            } else {
                                existing.push_str(remainder);
                                break;
                            }
                        } else if let Some(split_at) = remainder.find('\n') {
                            let (segment, rest) = remainder.split_at(split_at + 1);
                            existing.push_str(segment);
                            remainder = rest;
                            self.app.streaming_assistant_idx = None;
                        } else {
                            existing.push_str(remainder);
                            break;
                        }
                    }
                    _ => self.app.streaming_assistant_idx = None,
                }
            } else {
                self.app
                    .transcript
                    .push(TranscriptItem::AssistantText(String::new()));
                self.app.streaming_assistant_idx = Some(self.app.transcript.len() - 1);
            }
        }
    }

    pub(super) fn pin_transcript_to_bottom(&mut self) {
        self.app.scroll_state.follow = true;
    }

    #[cfg(test)]
    pub(crate) fn clear_transcript(&mut self) {
        self.app.transcript.clear();
        self.app.scroll_state = ratatui_widget_scrolling::ScrollState::new();
        self.app.streaming_assistant_idx = None;
    }

    pub(super) fn build_input_title(&self) -> Line<'static> {
        let config_read = self.config.read();
        let mut spans = vec![];

        let spinner = if self.app.llm_busy {
            SPINNER_FRAMES[self.app.spinner_index]
        } else {
            "•"
        };
        spans.push(Span::styled(
            format!("{spinner} "),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));

        let mut parts = vec![];
        let status = config_read.render_status_line(true);
        if !status.is_empty() {
            parts.push(status);
        }

        if let Some(session) = config_read.session.as_ref() {
            let usage = session.completion_usage();
            if !usage.is_empty() {
                parts.push(usage.to_string());
            }

            let (tokens, percent) = session.tokens_usage();
            if tokens > 0 {
                if percent > 0.0 {
                    parts.push(format!("Context: {}({:.0}%)", tokens, percent));
                } else {
                    parts.push(format!("Context: {}", tokens));
                }
            }
        }

        let text = if parts.is_empty() {
            "Input".to_string()
        } else {
            parts.join("   ")
        };
        spans.push(Span::styled(
            text,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));

        Line::from(spans)
    }

    pub(super) fn refresh_input_chrome(&mut self) {
        let llm_busy = self.app.llm_busy;
        let pending_message = self.app.pending_message.is_some();
        Self::refresh_input_chrome_from_state(
            &self.config,
            &mut self.app,
            llm_busy,
            pending_message,
        );
    }

    pub(super) fn refresh_input_chrome_from_state(
        _config: &GlobalConfig,
        app: &mut App,
        _llm_busy: bool,
        pending_message: bool,
    ) {
        let (input_style, cursor_style) = if pending_message {
            (
                Style::default().fg(Color::Yellow),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )
        } else if app.history_preview {
            (
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::REVERSED),
            )
        } else {
            (
                Style::default().fg(Color::Reset),
                Style::default().add_modifier(Modifier::REVERSED),
            )
        };
        app.input.set_style(input_style);
        app.input.set_cursor_style(cursor_style);
    }
}
