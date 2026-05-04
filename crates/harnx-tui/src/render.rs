use crate::types::Tui;
use crate::types::{
    App, ModalState, ToolCallBody, TranscriptItem, MAX_INPUT_HEIGHT, MIN_INPUT_HEIGHT,
    SPINNER_FRAMES,
};
use harnx_runtime::config::GlobalConfig;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

/// Options for `render_list_modal` — bundles the three metadata strings so the
/// function stays within clippy's `too_many_arguments` limit.
struct ListModalOpts<'a> {
    title: &'a str,
    footer: &'a str,
    /// Optional live-filter query. When `Some`, renders a `🔍 <query>█` search
    /// row above the list and a "No matches" placeholder when the list is empty.
    query: Option<&'a str>,
}

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

    /// Multi-line markdown body renderer — used for tool call bodies and
    /// tool results. Block-level constructs like fenced ```diff are handled
    /// by the whole-document parser. The dim base style is patched under
    /// each span so plain text still reads as dim.
    fn render_markdown_block(text: &str) -> Vec<Line<'static>> {
        let body_base = Style::default().add_modifier(Modifier::DIM);
        crate::render_helpers::markdown_lines(text, body_base)
    }

    /// Render a `ToolCall` transcript item: `→ tool_name` header followed
    /// by the body lines. Body rendering depends on its origin —
    /// `Markdown` (from a `call_template`) is rendered inline; `Yaml`
    /// (raw args, no template) is displayed verbatim, each line indented.
    fn render_tool_call(tool_name: &str, body: Option<&ToolCallBody>) -> Vec<Line<'static>> {
        let dim_gray = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);

        let mut lines = match body {
            Some(ToolCallBody::Markdown(_)) => Vec::new(),
            _ => {
                let header_text = format!("→ {tool_name}");
                Self::render_text_entry("", &header_text, dim_gray, false)
            }
        };
        match body {
            Some(ToolCallBody::Yaml(yaml)) => {
                for line in yaml.lines() {
                    lines.extend(Self::render_text_entry("", line, dim_gray, false));
                }
            }
            Some(ToolCallBody::Markdown(md)) => {
                lines.extend(Self::render_markdown_block(md));
            }
            None => {}
        }
        lines
    }

    fn render_meta_suffix(
        seq: Option<usize>,
        timestamp: Option<chrono::DateTime<chrono::Utc>>,
        show_seq: bool,
        show_ts: bool,
        use_utc: bool,
    ) -> Option<Span<'static>> {
        let mut parts = vec![];
        if show_seq {
            if let Some(n) = seq {
                parts.push(format!("[{n}]"));
            }
        }
        if show_ts {
            if let Some(ts) = timestamp {
                let formatted = if use_utc {
                    ts.format("%H:%M:%S").to_string()
                } else {
                    ts.with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string()
                };
                parts.push(formatted);
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(Span::styled(
            format!("  {}", parts.join(" ")),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ))
    }

    pub(super) fn render_entry(
        entry: &TranscriptItem,
        show_seq: bool,
        show_ts: bool,
        use_utc: bool,
    ) -> Vec<Line<'static>> {
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
            TranscriptItem::MutationNotice(text) => Self::render_text_entry(
                "",
                text,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
                false,
            ),
            TranscriptItem::UserText {
                text,
                seq,
                timestamp,
            } => {
                let mut lines = Self::render_text_entry(
                    "> ",
                    text,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    true,
                );
                if let Some(suffix) =
                    Self::render_meta_suffix(*seq, *timestamp, show_seq, show_ts, use_utc)
                {
                    if let Some(first_line) = lines.first_mut() {
                        first_line.spans.push(suffix);
                    }
                }
                lines
            }
            TranscriptItem::AssistantText {
                text,
                seq,
                timestamp,
            } => {
                // Render assistant messages as markdown so headings, lists,
                // code fences, and inline emphasis show their styling.
                // Streaming chunks rebuild this entry on every render — an
                // unclosed `**bold` mid-stream simply renders as literal
                // asterisks for the moment, then upgrades to bold once the
                // closing `**` arrives in a later chunk.
                let mut lines = crate::render_helpers::markdown_lines(text, Style::default());
                if let Some(suffix) =
                    Self::render_meta_suffix(*seq, *timestamp, show_seq, show_ts, use_utc)
                {
                    if lines.is_empty() {
                        lines.push(Line::from(""));
                    }
                    if let Some(first_line) = lines.first_mut() {
                        first_line.spans.push(suffix);
                    }
                }
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
            TranscriptItem::ToolResultMarkdown(text) => Self::render_markdown_block(text),
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
            TranscriptItem::ToolCall {
                tool_name,
                body,
                seq,
                timestamp,
            } => {
                let mut lines = vec![];
                if let Some(suffix) =
                    Self::render_meta_suffix(*seq, *timestamp, show_seq, show_ts, use_utc)
                {
                    lines.push(Line::from(suffix));
                }
                lines.extend(Self::render_tool_call(tool_name, body.as_ref()));
                lines
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

        let show_seq = self.app.show_sequence_numbers;
        let show_ts = self.app.show_timestamps;
        let use_utc = self.app.use_utc_timestamps;
        let selected_range = if let Some(f) = self.app.transcript_focus {
            let start = self.app.transcript_selection_anchor.unwrap_or(f).min(f);
            let end = self.app.transcript_selection_anchor.unwrap_or(f).max(f);
            Some(start..=end)
        } else {
            None
        };

        if self.app.scroll_to_focused_item {
            if let Some(focus) = self.app.transcript_focus {
                let position = self.app.scroll_state.scroll_position_to_show_item(
                    focus,
                    chunks[0].width,
                    chunks[0].height as usize,
                    self.app.transcript.len(),
                );
                self.app.scroll_state.position = position;
            }
            self.app.scroll_to_focused_item = false;
        }

        let transcript_entries: Vec<Vec<Line<'static>>> = if self.app.transcript.is_empty() {
            vec![vec![Line::from(Span::raw(""))]]
        } else {
            self.app
                .transcript
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let mut lines = Self::render_entry(entry, show_seq, show_ts, use_utc);
                    if let Some(range) = &selected_range {
                        if range.contains(&i) {
                            for line in &mut lines {
                                line.style = line.style.add_modifier(Modifier::REVERSED);
                                for span in &mut line.spans {
                                    span.style = span.style.add_modifier(Modifier::REVERSED);
                                }
                            }
                        }
                    }
                    lines
                })
                .collect()
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

        // Render detail view if open (exclusive — returns early)
        if self.app.detail_view_open {
            self.render_detail_view(frame, size);
            return;
        }

        // Render confirmation modal on top of everything else
        if let Some(modal) = &self.app.modal {
            self.render_modal(frame, size, modal);
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
                    Some(TranscriptItem::AssistantText { text: existing, .. }) => {
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
                self.app.transcript.push(TranscriptItem::AssistantText {
                    text: String::new(),
                    seq: None,
                    timestamp: Some(chrono::Utc::now()),
                });
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
        } else if app.transcript_focus.is_some() {
            // Transcript item is focused — input is inactive; hide the cursor.
            (
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
                Style::default(), // no REVERSED = invisible cursor
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

    /// Render a centered confirmation modal overlay.
    fn render_modal(
        &self,
        frame: &mut Frame<'_>,
        screen_size: ratatui::layout::Rect,
        modal: &ModalState,
    ) {
        match modal {
            ModalState::ConfirmDelete { from, to } => {
                let prompt_text = if from == to {
                    format!("Delete entry {}? [y/N]", from)
                } else {
                    format!("Delete entries {}–{}? [y/N]", from, to)
                };
                self.render_simple_modal(frame, screen_size, &prompt_text);
            }
            ModalState::ConfirmRewind { seq, .. } => {
                let prompt_text = format!("Rewind to entry {}? [y/N]", seq);
                self.render_simple_modal(frame, screen_size, &prompt_text);
            }
            ModalState::AgentPicker {
                agents,
                selected,
                query,
            } => {
                let title = "Select Agent";
                let footer = "type to filter  ↑↓ navigate  Enter select  Esc cancel";
                let items = ModalState::filtered_agents(agents, query);
                self.render_list_modal(
                    frame,
                    screen_size,
                    &items,
                    *selected,
                    ListModalOpts {
                        title,
                        footer,
                        query: Some(query),
                    },
                );
            }
            ModalState::SessionPicker {
                sessions, selected, ..
            } => {
                let title = "Select Session";
                let footer = "↑↓ navigate  Enter select  Esc cancel";
                let mut items: Vec<String> = vec!["✦ New session".to_string()];
                items.extend(sessions.iter().map(|s| {
                    let branch = s.git_branch.as_deref().unwrap_or("");
                    let cwd = s.working_dir.as_deref().unwrap_or("");
                    let parts: Vec<&str> =
                        cwd.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
                    let cwd_tail = if parts.len() >= 2 {
                        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
                    } else if parts.len() == 1 {
                        parts[0].to_string()
                    } else {
                        String::new()
                    };
                    format!("{}  {}  {}", s.id, branch, cwd_tail)
                }));
                self.render_list_modal(
                    frame,
                    screen_size,
                    &items,
                    *selected,
                    ListModalOpts {
                        title,
                        footer,
                        query: None,
                    },
                );
            }
        }
    }

    fn render_simple_modal(
        &self,
        frame: &mut Frame<'_>,
        screen_size: ratatui::layout::Rect,
        prompt_text: &str,
    ) {
        let prompt_len = prompt_text.len() as u16;
        let modal_width = (prompt_len + 6).min(screen_size.width.saturating_sub(4));
        let modal_height = 3u16;

        let modal_x = (screen_size.width.saturating_sub(modal_width)) / 2;
        let modal_y = (screen_size.height.saturating_sub(modal_height)) / 2;
        let modal_area = ratatui::layout::Rect::new(modal_x, modal_y, modal_width, modal_height);

        frame.render_widget(ratatui::widgets::Clear, modal_area);

        let modal = Paragraph::new(Line::from(Span::styled(
            prompt_text,
            Style::default().fg(Color::Reset),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Reset)),
        );

        frame.render_widget(modal, modal_area);
    }

    fn render_list_modal(
        &self,
        frame: &mut Frame<'_>,
        screen_size: ratatui::layout::Rect,
        items: &[String],
        selected: usize,
        opts: ListModalOpts<'_>,
    ) {
        let ListModalOpts {
            title,
            footer,
            query,
        } = opts;
        let max_item_len = items.iter().map(|s| s.len()).max().unwrap_or(0);
        // Search row looks like "🔍 <query>█"; ensure minimum comfortable width.
        let query_row_width: u16 = query.map(|q| (q.len() as u16 + 4).max(24)).unwrap_or(0);
        let modal_width = (max_item_len as u16 + 6)
            .max(title.len() as u16 + 4)
            .max(footer.len() as u16 + 4)
            .max(query_row_width)
            .min(screen_size.width.saturating_sub(4));

        let visible_count = items.len().min(10);
        let search_rows: u16 = if query.is_some() { 2 } else { 0 };
        let list_rows = visible_count.max(usize::from(query.is_some() && items.is_empty())) as u16;
        let modal_height = (list_rows + 4 + search_rows).min(screen_size.height.saturating_sub(4));

        let modal_x = (screen_size.width.saturating_sub(modal_width)) / 2;
        let modal_y = (screen_size.height.saturating_sub(modal_height)) / 2;
        let modal_area = ratatui::layout::Rect::new(modal_x, modal_y, modal_width, modal_height);

        frame.render_widget(ratatui::widgets::Clear, modal_area);

        let mut lines = Vec::new();

        // Search input row with magnifying-glass icon and simulated cursor.
        if let Some(q) = query {
            lines.push(Line::from(vec![
                Span::styled("🔍 ", Style::default().fg(Color::Yellow)),
                Span::styled(q.to_string(), Style::default().fg(Color::Reset)),
                Span::styled("█", Style::default().fg(Color::Yellow)),
            ]));
            lines.push(Line::from(""));
        }

        if items.is_empty() && query.is_some() {
            lines.push(Line::from(Span::styled(
                "No matches",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            let mut start = selected.saturating_sub(4);
            let end = (start + 10).min(items.len());
            start = end.saturating_sub(10);

            for (i, item) in items.iter().enumerate().skip(start).take(10) {
                let style = if i == selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(item.clone(), style)));
            }
        }

        // Empty line
        lines.push(Line::from(""));

        // Footer hint
        lines.push(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
        )));

        let modal = Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Reset)),
        );

        frame.render_widget(modal, modal_area);
    }

    /// Render the detail view overlay for the selected transcript range.
    /// Called from draw() when self.app.detail_view_open is true.
    pub(super) fn render_entry_detail(entry: &TranscriptItem) -> Vec<Line<'static>> {
        let label_style = Style::default().fg(Color::DarkGray);
        let mut lines = Vec::new();

        macro_rules! push_field {
            ($key:expr, $value:expr) => {
                if $value.contains('\n') {
                    lines.push(Line::from(vec![Span::styled(
                        format!("{}:", $key),
                        label_style,
                    )]));
                    for line in $value.lines() {
                        lines.push(Line::from(Span::raw(line.to_string())));
                    }
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}: ", $key), label_style),
                        Span::raw($value.to_string()),
                    ]));
                }
            };
        }

        match entry {
            TranscriptItem::ToolCall {
                tool_name,
                body,
                seq,
                timestamp,
            } => {
                lines.push(Line::from(Span::styled("── tool call ──", label_style)));
                if let Some(s) = seq {
                    push_field!("seq", &s.to_string());
                }
                if let Some(ts) = timestamp {
                    push_field!("timestamp", &ts.to_rfc3339());
                }
                push_field!("tool_name", tool_name);
                if let Some(b) = body {
                    match b {
                        crate::types::ToolCallBody::Yaml(y) => push_field!("body (yaml)", y),
                        crate::types::ToolCallBody::Markdown(m) => {
                            push_field!("body (markdown)", m)
                        }
                    }
                }
            }
            TranscriptItem::UserText {
                text,
                seq,
                timestamp,
            } => {
                lines.push(Line::from(Span::styled("── user ──", label_style)));
                if let Some(s) = seq {
                    push_field!("seq", &s.to_string());
                }
                if let Some(ts) = timestamp {
                    push_field!("timestamp", &ts.to_rfc3339());
                }
                push_field!("text", text);
            }
            TranscriptItem::AssistantText {
                text,
                seq,
                timestamp,
            } => {
                lines.push(Line::from(Span::styled("── assistant ──", label_style)));
                if let Some(s) = seq {
                    push_field!("seq", &s.to_string());
                }
                if let Some(ts) = timestamp {
                    push_field!("timestamp", &ts.to_rfc3339());
                }
                push_field!("text", text);
            }
            TranscriptItem::ToolResultMarkdown(text) => {
                lines.push(Line::from(Span::styled("── tool result ──", label_style)));
                push_field!("result", text);
            }
            TranscriptItem::SourceHeading(source) => {
                lines.push(Line::from(Span::styled("── source ──", label_style)));
                push_field!("source", &crate::render_helpers::source_heading(source));
            }
            TranscriptItem::SystemText(text) => {
                lines.push(Line::from(Span::styled("── system ──", label_style)));
                push_field!("text", text);
            }
            TranscriptItem::ErrorText(text) => {
                lines.push(Line::from(Span::styled("── error ──", label_style)));
                push_field!("error", text);
            }
            TranscriptItem::ThoughtText(text) => {
                lines.push(Line::from(Span::styled("── thinking ──", label_style)));
                push_field!("thought", text);
            }
            TranscriptItem::StatusLine(text) => {
                lines.push(Line::from(Span::styled("── status ──", label_style)));
                push_field!("status", text);
            }
            TranscriptItem::UsageLine(text) => {
                lines.push(Line::from(Span::styled("── usage ──", label_style)));
                push_field!("usage", text);
            }
            TranscriptItem::Plan(plan) => {
                lines.push(Line::from(Span::styled("── plan ──", label_style)));
                for (i, p) in plan.iter().enumerate() {
                    push_field!(
                        &format!("entry[{}]", i),
                        &format!("{} [{}]", p.content, p.status)
                    );
                }
            }
            TranscriptItem::AttachmentHeader(text) => {
                lines.push(Line::from(Span::styled("── attachment ──", label_style)));
                push_field!("text", text);
            }
            TranscriptItem::AttachmentItem(text) => {
                lines.push(Line::from(Span::styled(
                    "── attachment item ──",
                    label_style,
                )));
                push_field!("text", text);
            }
            TranscriptItem::AttachmentPreviewLine(text) => {
                lines.push(Line::from(Span::styled(
                    "── attachment preview ──",
                    label_style,
                )));
                push_field!("text", text);
            }
            TranscriptItem::MutationNotice(text) => {
                lines.push(Line::from(Span::styled("── notice ──", label_style)));
                push_field!("text", text);
            }
        }
        lines
    }

    pub(super) fn render_detail_view(
        &mut self,
        frame: &mut Frame<'_>,
        size: ratatui::layout::Rect,
    ) {
        // Clear the full area first
        frame.render_widget(ratatui::widgets::Clear, size);

        // Split vertically: content area + footer
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(size);

        // Build display content.
        //
        // Primary: raw session-log YAML (same text .edit message would open in
        // the editor) — one Vec<Line> per YAML document, separated by a "---"
        // divider line.
        //
        // Fallback: render_entry_detail() for items that have no seq number or
        // when no session is active.
        let (entries_as_vec, title): (Vec<Vec<Line<'static>>>, String) =
            if let Some(yaml) = &self.app.detail_view_raw_yaml {
                // Split on the same separator edit_message_range joins with.
                let docs: Vec<&str> = yaml.split("\n---\n").collect();
                let doc_count = docs.len();
                let mut entries: Vec<Vec<Line<'static>>> = Vec::new();
                for (i, doc) in docs.into_iter().enumerate() {
                    let doc_lines: Vec<Line<'static>> = doc
                        .lines()
                        .map(|l| Line::from(Span::raw(l.to_string())))
                        .collect();
                    entries.push(doc_lines);
                    if i + 1 < doc_count {
                        // Visual separator between documents
                        entries.push(vec![Line::from(Span::styled(
                            "---",
                            Style::default().fg(Color::DarkGray),
                        ))]);
                    }
                }
                let title = if doc_count == 1 {
                    "Detail".to_string()
                } else {
                    format!("Detail ({doc_count} entries)")
                };
                (entries, title)
            } else {
                // Fallback: no session / no seq — render TUI fields verbatim
                let (from, to) = self.app.selected_transcript_range();
                let mut entries = Vec::new();
                for i in from..=to {
                    if i < self.app.transcript.len() {
                        let entry = &self.app.transcript[i];
                        entries.push(Self::render_entry_detail(entry));
                        if i < to {
                            entries.push(vec![Line::from("")]);
                        }
                    }
                }
                let title = if from == to {
                    "Detail".to_string()
                } else {
                    format!("Detail ({from}–{to})")
                };
                (entries, title)
            };

        // Create block with border and title
        let block = Block::default().borders(Borders::ALL).title(title.as_str());

        // Get inner area for content
        let inner_area = block.inner(chunks[0]);

        // Render the block into the content chunk
        frame.render_widget(block, chunks[0]);

        // Render the scrollable content
        self.app
            .detail_view_scroll
            .render(frame, inner_area, &entries_as_vec, |lines| {
                let paragraph = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
                let height = paragraph.line_count(inner_area.width);
                (height, paragraph)
            });

        // Clamp position to the freshly-updated last_max_position
        self.app.detail_view_scroll.position = self
            .app
            .detail_view_scroll
            .position
            .min(self.app.detail_view_scroll.last_max_position);

        // Render footer
        let footer =
            Paragraph::new(" ↑↓/scroll — ESC to close").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, chunks[1]);
    }
}
