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

    /// 3-space indent + multi-line markdown body, used for tool results
    /// where block-level constructs like fenced ```diff need the
    /// whole-document parser to see them. Each parsed ratatui line gets
    /// the indent prefixed and the dim base style patched under each
    /// span so plain text still reads as dim.
    fn render_indented_markdown_block(text: &str) -> Vec<Line<'static>> {
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
            TranscriptItem::ToolResultMarkdown(text) => Self::render_indented_markdown_block(text),
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
                            if let Some(first_line) = lines.first_mut() {
                                first_line.style =
                                    first_line.style.add_modifier(Modifier::REVERSED);
                                for span in &mut first_line.spans {
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

        // Render action menu if open
        if self.app.action_menu_open && self.app.transcript_focus.is_some() {
            self.render_action_menu(frame, size);
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

    /// Render a centered action menu overlay.
    fn render_action_menu(&self, frame: &mut Frame<'_>, screen_size: ratatui::layout::Rect) {
        let lines = vec![
            Line::from(vec![
                Span::styled("[e] ", Style::default().fg(Color::Yellow)),
                Span::raw("Edit     "),
                Span::styled("[d] ", Style::default().fg(Color::Yellow)),
                Span::raw("Delete   "),
                Span::styled("[i] ", Style::default().fg(Color::Yellow)),
                Span::raw("Insert"),
            ]),
            Line::from(vec![
                Span::styled("[c] ", Style::default().fg(Color::Yellow)),
                Span::raw("Copy     "),
                Span::styled("[r] ", Style::default().fg(Color::Yellow)),
                Span::raw("Rewind   "),
                Span::styled("[Esc] ", Style::default().fg(Color::Yellow)),
                Span::raw("Cancel"),
            ]),
        ];

        let modal_width = 40;
        let modal_height = 4; // 2 lines of text + borders

        // Center the modal
        let modal_x = (screen_size.width.saturating_sub(modal_width)) / 2;
        let modal_y = (screen_size.height.saturating_sub(modal_height)) / 2;
        let modal_area = ratatui::layout::Rect::new(modal_x, modal_y, modal_width, modal_height);

        // Clear the area behind the modal
        frame.render_widget(ratatui::widgets::Clear, modal_area);

        // Render the modal box
        let modal = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Actions")
                .border_style(Style::default().fg(Color::Reset)),
        );

        frame.render_widget(modal, modal_area);
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
            ModalState::AgentPicker { agents, selected } => {
                let title = "Select Agent";
                let footer = "↑↓ navigate  Enter select  Esc cancel";
                let items: Vec<String> = agents.clone();
                self.render_list_modal(frame, screen_size, title, footer, &items, *selected);
            }
            ModalState::SessionPicker {
                sessions, selected, ..
            } => {
                let title = "Select Session";
                let footer = "↑↓ navigate  Enter select  Esc new session";
                let items: Vec<String> = sessions
                    .iter()
                    .map(|s| {
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
                    })
                    .collect();
                self.render_list_modal(frame, screen_size, title, footer, &items, *selected);
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
        title: &str,
        footer: &str,
        items: &[String],
        selected: usize,
    ) {
        let max_item_len = items.iter().map(|s| s.len()).max().unwrap_or(0);
        let modal_width = (max_item_len as u16 + 6)
            .max(title.len() as u16 + 4)
            .max(footer.len() as u16 + 4)
            .min(screen_size.width.saturating_sub(4));

        let visible_count = items.len().min(10);
        let modal_height = (visible_count as u16 + 4).min(screen_size.height.saturating_sub(4));

        let modal_x = (screen_size.width.saturating_sub(modal_width)) / 2;
        let modal_y = (screen_size.height.saturating_sub(modal_height)) / 2;
        let modal_area = ratatui::layout::Rect::new(modal_x, modal_y, modal_width, modal_height);

        frame.render_widget(ratatui::widgets::Clear, modal_area);

        let mut lines = Vec::new();

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
}
