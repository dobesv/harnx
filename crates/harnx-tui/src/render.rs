use crate::markdown_render::{MarkdownBlockData, RenderedEntry};
use crate::subagent_render::{render_subagent_detail, render_subagent_row};
use crate::types::{
    App, ModalState, ToolCallBody, TranscriptItem, MAX_INPUT_HEIGHT, MIN_INPUT_HEIGHT,
    SPINNER_FRAMES,
};
use crate::types::{RenderEntryState, Tui};
use harnx_core::event::{AgentEvent, SessionEvent, TurnEvent};
use harnx_runtime::config::GlobalConfig;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use syntect::highlighting::Theme;

fn dim_style(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::DIM)
}

fn plan_detail_lines(plan: &[harnx_core::event::PlanEntry], label: Style) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("── plan ──", label))];
    lines.extend(plan.iter().enumerate().map(|(index, entry)| {
        Line::from(vec![
            Span::styled(format!("entry[{index}]: "), label),
            Span::raw(format!("{} [{}]", entry.content, entry.status)),
        ])
    }));
    lines
}

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
    pub(super) fn render_model_source_change(&mut self, event: &AgentEvent) -> bool {
        let model = match event {
            AgentEvent::Turn(TurnEvent::ModelFallback { to, .. })
            | AgentEvent::Session(SessionEvent::ModelChanged { to, .. }) => to,
            _ => return false,
        };
        let source = self.app.last_ui_output_source.clone().map(|mut source| {
            source.model = Some(model.clone());
            source
        });
        self.render_ui_output_heading(source.as_ref(), false);
        true
    }

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

    /// Render a `ToolCall` transcript item: `→ tool_name` header followed
    /// by the body lines. Body rendering depends on its origin —
    /// `Markdown` (from a `call_template`) is rendered inline; `Yaml`
    /// (raw args, no template) is displayed verbatim, each line indented.
    fn render_tool_call(
        tool_name: &str,
        body: Option<&ToolCallBody>,
        width: u16,
        theme: Option<&Theme>,
    ) -> RenderedEntry {
        let dim_gray = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);

        match body {
            Some(ToolCallBody::Markdown(md)) => {
                // Markdown body is the tool description itself — header suppressed
                // intentionally (the markdown content replaces the "→ tool_name" line).
                crate::markdown_render::render_markdown(md, dim_gray, width, theme)
            }
            Some(ToolCallBody::Yaml(yaml)) => {
                let mut lines = vec![];
                let header_text = format!("→ {tool_name}");
                lines.extend(Self::render_text_entry("", &header_text, dim_gray, false));
                for line in yaml.lines() {
                    lines.extend(Self::render_text_entry("", line, dim_gray, false));
                }
                RenderedEntry::from_lines(lines, width)
            }
            None => {
                let header_text = format!("→ {tool_name}");
                let lines = Self::render_text_entry("", &header_text, dim_gray, false);
                RenderedEntry::from_lines(lines, width)
            }
        }
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
        entry: &mut TranscriptItem,
        show_seq: bool,
        show_ts: bool,
        use_utc: bool,
        width: u16,
        state: RenderEntryState,
        theme: Option<&Theme>,
    ) -> RenderedEntry {
        match entry {
            TranscriptItem::SourceHeading(source) => {
                let lines = Self::render_text_entry(
                    "",
                    &crate::render_helpers::source_heading(source),
                    dim_style(Color::DarkGray),
                    false,
                );
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::CompactionMarker {
                text, summary_text, ..
            } => {
                let mut lines = Self::render_text_entry(
                    "",
                    text,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    false,
                );
                if !summary_text.is_empty() {
                    lines.extend(Self::render_text_entry(
                        "",
                        summary_text,
                        dim_style(Color::DarkGray),
                        false,
                    ));
                }
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::SystemText(text) => {
                let lines = Self::render_text_entry("", text, dim_style(Color::DarkGray), false);
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::MutationNotice(text) => {
                let lines = Self::render_text_entry("", text, dim_style(Color::Yellow), false);
                RenderedEntry::from_lines(lines, width)
            }
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
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::AssistantText {
                text,
                seq,
                timestamp,
                rendered_cache,
            } => {
                if let Some((w, ss, sts, utc, cached)) = rendered_cache.as_ref() {
                    if *w == width && *ss == show_seq && *sts == show_ts && *utc == use_utc {
                        return cached.clone();
                    }
                }
                // Render assistant messages as markdown so headings, lists,
                // code fences, and inline emphasis show their styling.
                // Streaming chunks rebuild this entry on every render — an
                // unclosed `**bold` mid-stream simply renders as literal
                // asterisks for the moment, then upgrades to bold once the
                // closing `**` arrives in a later chunk.
                let mut entry =
                    crate::markdown_render::render_markdown(text, Style::default(), width, theme);
                if let Some(suffix) =
                    Self::render_meta_suffix(*seq, *timestamp, show_seq, show_ts, use_utc)
                {
                    // Attach suffix to first line of first Paragraph block.
                    // If no paragraph block exists (all tables), insert a
                    // suffix-only paragraph so metadata is never silently dropped.
                    let mut attached = false;
                    for block in &mut entry.blocks {
                        if let MarkdownBlockData::Paragraph { lines, .. } = block {
                            if let Some(first_line) = lines.first_mut() {
                                first_line.spans.push(suffix.clone());
                                attached = true;
                            }
                            break;
                        }
                    }
                    if !attached {
                        let suffix_line = Line::from(suffix);
                        entry.total_height += 1;
                        entry.blocks.push(MarkdownBlockData::Paragraph {
                            lines: vec![suffix_line],
                            height: 1,
                        });
                    }
                }
                // Match the prior trailing-spacing rule: pad after a
                // single-line message (so the next entry has breathing
                // room) but skip the pad when the text already contains
                // newlines.
                if !text.contains('\n') {
                    entry.blocks.push(MarkdownBlockData::Paragraph {
                        lines: vec![Line::from("")],
                        height: 1,
                    });
                    entry.total_height += 1;
                }
                if !state.skip_cache {
                    *rendered_cache = Some((width, show_seq, show_ts, use_utc, entry.clone()));
                }
                entry
            }
            TranscriptItem::ErrorText(text) => {
                let lines = Self::render_text_entry(
                    "error: ",
                    text,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    true,
                );
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::ThoughtText(text) => {
                let lines = Self::render_text_entry(
                    "",
                    &format!("<think>{text}</think>"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    false,
                );
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::ToolResultMarkdown {
                text,
                rendered_cache,
            } => {
                if let Some((w, ss, sts, utc, cached)) = rendered_cache.as_ref() {
                    if *w == width && *ss == show_seq && *sts == show_ts && *utc == use_utc {
                        return cached.clone();
                    }
                }
                let body_base = Style::default().add_modifier(Modifier::DIM);
                let entry = crate::markdown_render::render_markdown(text, body_base, width, theme);
                if !state.skip_cache {
                    *rendered_cache = Some((width, show_seq, show_ts, use_utc, entry.clone()));
                }
                entry
            }
            TranscriptItem::StatusLine(text) => {
                let lines = Self::render_text_entry(
                    "",
                    text,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    false,
                );
                RenderedEntry::from_lines(lines, width)
            }
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
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::UsageLine(text) => {
                let lines = Self::render_text_entry(
                    "",
                    text,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    false,
                );
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::ToolCall {
                tool_name,
                body,
                seq,
                timestamp,
                rendered_cache,
            } => {
                if let Some((w, ss, sts, utc, cached)) = rendered_cache.as_ref() {
                    if *w == width && *ss == show_seq && *sts == show_ts && *utc == use_utc {
                        return cached.clone();
                    }
                }
                let mut entry = Self::render_tool_call(tool_name, body.as_ref(), width, theme);
                if let Some(suffix) =
                    Self::render_meta_suffix(*seq, *timestamp, show_seq, show_ts, use_utc)
                {
                    // Prepend a line with the suffix
                    let suffix_line = Line::from(suffix);
                    entry.total_height += 1;
                    entry.blocks.insert(
                        0,
                        MarkdownBlockData::Paragraph {
                            lines: vec![suffix_line],
                            height: 1,
                        },
                    );
                }
                if !state.skip_cache {
                    *rendered_cache = Some((width, show_seq, show_ts, use_utc, entry.clone()));
                }
                entry
            }
            item @ TranscriptItem::SubAgentSession { .. } => {
                render_subagent_row(item, state.spinner_index, width)
            }
            TranscriptItem::AttachmentHeader(text) => {
                let lines = Self::render_text_entry(
                    "",
                    &format!("{text}:"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    false,
                );
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::AttachmentItem(text) => {
                let lines = Self::render_text_entry(
                    "  - ",
                    text,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    false,
                );
                RenderedEntry::from_lines(lines, width)
            }
            TranscriptItem::AttachmentPreviewLine(text) => {
                let lines = Self::render_text_entry(
                    "      ",
                    text,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                    false,
                );
                RenderedEntry::from_lines(lines, width)
            }
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

        let transcript_entries = self.prepare_transcript_entries(
            chunks[0].width,
            show_seq,
            show_ts,
            use_utc,
            &selected_range,
        );

        self.app
            .scroll_state
            .render(frame, chunks[0], &transcript_entries, |entry| {
                (entry.total_height as usize, entry.clone())
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

        if self.render_exclusive_transcript(frame, size) {
            if let Some(modal) = &self.app.modal.clone() {
                self.render_modal(frame, size, modal);
            }
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

    /// Append a streamed assistant text chunk to the open streaming run,
    /// which is always the trailing `AssistantText` transcript item. If no
    /// run is open — at turn start, or because an interleaving item (tool
    /// call, tool result, notice, source heading, …) became the trailing
    /// item — a fresh `AssistantText` is started. An interleaving item thus
    /// breaks the surrounding text into separate blocks for free, with no
    /// per-event bookkeeping.
    pub(super) fn append_streaming_assistant_chunk(&mut self, chunk: &str, is_sub_agent: bool) {
        if chunk.is_empty() {
            return;
        }
        let open = self.app.streaming_open
            && matches!(
                self.app.transcript.last(),
                Some(TranscriptItem::AssistantText { .. })
            );
        if !open {
            self.app.transcript.push(TranscriptItem::AssistantText {
                text: String::new(),
                seq: None,
                timestamp: Some(chrono::Utc::now()),
                rendered_cache: None,
            });
            self.app.streaming_open = true;
        }
        if let Some(TranscriptItem::AssistantText {
            text,
            rendered_cache,
            ..
        }) = self.app.transcript.last_mut()
        {
            text.push_str(chunk);
            // Invalidate the cached render so the appended text repaints.
            *rendered_cache = None;
        }
        if !is_sub_agent {
            self.app.main_streamed_text_idx = Some(self.app.transcript.len() - 1);
        }
        self.pin_transcript_to_bottom();
    }

    pub(super) fn pin_transcript_to_bottom(&mut self) {
        self.app.scroll_state.follow = true;
    }

    #[cfg(test)]
    pub(crate) fn clear_transcript(&mut self) {
        self.app.transcript.clear();
        self.subagent_rows_dirty = true;
        self.app.scroll_state = ratatui_widget_scrolling::ScrollState::new();
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
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
            ModalState::ConfirmToolUse {
                tool_name,
                input_preview,
                reason,
            } => {
                self.render_tool_confirm_modal(
                    frame,
                    screen_size,
                    tool_name,
                    input_preview,
                    reason.as_deref(),
                );
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
                sessions,
                selected,
                error,
                ..
            } => {
                let title = "Select Session";
                let footer = "↑↓ navigate  Enter select  Esc cancel";
                let mut items: Vec<String> = vec!["✦ New session".to_string()];
                items.extend(sessions.iter().map(|session| session.picker_label()));
                // Prepend error message if present (visible in picker)
                if let Some(err) = error {
                    items.insert(0, format!("⚠ {}", err));
                }
                self.render_list_modal(
                    frame,
                    screen_size,
                    &items,
                    // Adjust selected index if error message is prepended
                    session_picker_highlight_index(*selected, error.is_some()),
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

    /// Multi-line confirmation modal for a `PreToolUse` "ask" gate. Shows the
    /// tool name, optional reason, optional argument preview, and a [y/N] prompt.
    fn render_tool_confirm_modal(
        &self,
        frame: &mut Frame<'_>,
        screen_size: ratatui::layout::Rect,
        tool_name: &str,
        input_preview: &str,
        reason: Option<&str>,
    ) {
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("Allow tool '{tool_name}'?"),
            Style::default()
                .fg(Color::Reset)
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            lines.push(Line::from(vec![
                Span::styled("Reason: ", dim),
                Span::styled(r.to_string(), Style::default().fg(Color::Reset)),
            ]));
        }
        if !input_preview.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("Input: ", dim),
                Span::styled(input_preview.to_string(), dim),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "[y] allow   [n] deny   (Enter/Esc denies)",
            dim,
        )));

        let content_width = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.chars().count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0) as u16;
        let modal_width = (content_width + 4)
            .max(24)
            .min(screen_size.width.saturating_sub(4));
        let modal_height = (lines.len() as u16 + 2).min(screen_size.height.saturating_sub(2));

        let modal_x = (screen_size.width.saturating_sub(modal_width)) / 2;
        let modal_y = (screen_size.height.saturating_sub(modal_height)) / 2;
        let modal_area = ratatui::layout::Rect::new(modal_x, modal_y, modal_width, modal_height);

        frame.render_widget(ratatui::widgets::Clear, modal_area);
        let modal = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Tool confirmation")
                .border_style(Style::default().fg(Color::Yellow)),
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
                rendered_cache: _,
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
                rendered_cache: _,
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
            TranscriptItem::ToolResultMarkdown { text, .. } => {
                lines.push(Line::from(Span::styled("── tool result ──", label_style)));
                push_field!("result", text);
            }
            TranscriptItem::SourceHeading(source) => {
                lines.push(Line::from(Span::styled("── source ──", label_style)));
                push_field!("source", &crate::render_helpers::source_heading(source));
            }
            TranscriptItem::CompactionMarker {
                text,
                summary_text,
                from_seq,
                to_seq,
                detail_text,
            } => {
                lines.push(Line::from(Span::styled(
                    "── compacted session ──",
                    label_style,
                )));
                push_field!("text", text);
                if !summary_text.is_empty() {
                    push_field!("summary", summary_text);
                }
                let seq_range = match (from_seq, to_seq) {
                    (Some(from), Some(to)) => format!("{from}–{to}"),
                    _ => "n/a".to_string(),
                };
                push_field!("seq_range", seq_range);
                push_field!("detail", detail_text);
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
            TranscriptItem::Plan(plan) => lines.extend(plan_detail_lines(plan, label_style)),
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
            item @ TranscriptItem::SubAgentSession { .. } => {
                lines.extend(render_subagent_detail(item));
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

        // Build display content for the selected root or child transcript item,
        // or for a textual information overlay.
        let (entries_as_vec, title) = crate::detail_view::detail_view_content(&self.app);

        // Create block with horizontal (top + bottom) borders and a title.
        //
        // Left/right borders are intentionally omitted: terminal text selection
        // of multi-line content would otherwise capture the vertical `|` border
        // glyphs on every line, polluting copied text (issue #710). Horizontal
        // borders are kept — the top border anchors the title and the bottom
        // border separates the content from the footer — without affecting
        // multi-line copy, since they never appear inside the selected lines.
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .title(title.as_str());

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
        let footer_text = crate::detail_view::detail_view_footer_text(&self.app);
        let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(footer, chunks[1]);
    }

    /// Build the rendered transcript entries for the given viewport width, applying
    /// cache invalidation, per-item render, and selection highlighting. Called from
    /// both the main view and the browsing view so the logic lives in one place.
    fn prepare_transcript_entries(
        &mut self,
        width: u16,
        show_seq: bool,
        show_ts: bool,
        use_utc: bool,
        selected_range: &Option<std::ops::RangeInclusive<usize>>,
    ) -> Vec<RenderedEntry> {
        // Invalidate caches when the viewport width changes.
        if Some(width) != self.app.cache_valid_width {
            for item in &mut self.app.transcript {
                match item {
                    TranscriptItem::AssistantText { rendered_cache, .. } => *rendered_cache = None,
                    TranscriptItem::ToolResultMarkdown { rendered_cache, .. } => {
                        *rendered_cache = None
                    }
                    TranscriptItem::ToolCall { rendered_cache, .. } => *rendered_cache = None,
                    _ => {}
                }
            }
            self.app.cache_valid_width = Some(width);
        }

        if self.app.transcript.is_empty() {
            return vec![RenderedEntry::from_lines(
                vec![Line::from(Span::raw(""))],
                width,
            )];
        }

        // The open streaming run is always the trailing item; skip its render
        // cache so in-progress text repaints each frame.
        let streaming_idx = self
            .app
            .streaming_open
            .then(|| self.app.transcript.len().checked_sub(1))
            .flatten();
        self.app
            .transcript
            .iter_mut()
            .enumerate()
            .map(|(i, entry)| {
                let mut rendered = Self::render_entry(
                    entry,
                    show_seq,
                    show_ts,
                    use_utc,
                    width,
                    RenderEntryState::new(Some(i) == streaming_idx, self.app.spinner_index),
                    self.code_theme.as_ref(),
                );
                if let Some(range) = selected_range {
                    if range.contains(&i) {
                        rendered.reverse_style();
                    }
                }
                rendered
            })
            .collect()
    }

    pub(super) fn render_browsing_view(
        &mut self,
        frame: &mut Frame<'_>,
        size: ratatui::layout::Rect,
    ) {
        // Clear the full area first
        frame.render_widget(ratatui::widgets::Clear, size);

        // Split vertically: content area + footer (2 rows)
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
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
                // Prime the browsing-view height cache from the main scroll state so that
                // scroll_position_to_show_item has accurate per-item heights even on the
                // very first render of the browsing overlay (before it has rendered anything
                // itself). Without this, the cache defaults to height=1 per item, producing
                // a wildly incorrect scroll position for multi-line transcript items.
                self.app
                    .browsing_view_scroll
                    .copy_height_cache_from(&self.app.scroll_state);
                let position = self.app.browsing_view_scroll.scroll_position_to_show_item(
                    focus,
                    chunks[0].width,
                    chunks[0].height as usize,
                    self.app.transcript.len(),
                );
                self.app.browsing_view_scroll.position = position;
            }
            self.app.scroll_to_focused_item = false;
        }

        let transcript_entries = self.prepare_transcript_entries(
            chunks[0].width,
            show_seq,
            show_ts,
            use_utc,
            &selected_range,
        );

        self.app
            .browsing_view_scroll
            .render(frame, chunks[0], &transcript_entries, |entry| {
                (entry.total_height as usize, entry.clone())
            });

        // Clamp position to the freshly-updated last_max_position
        self.app.browsing_view_scroll.position = self
            .app
            .browsing_view_scroll
            .position
            .min(self.app.browsing_view_scroll.last_max_position);

        // Count navigable items and find display index
        let mut navigable_count = 0;
        let mut display_idx = 0;
        for (i, item) in self.app.transcript.iter().enumerate() {
            if item.is_navigable() {
                navigable_count += 1;
                if Some(i) == self.app.transcript_focus {
                    display_idx = navigable_count;
                }
            }
        }

        // Split footer into two rows
        let footer_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(chunks[1]);

        let counter_text = format!(" Item {} of {} (navigable)", display_idx, navigable_count);
        let counter_para = Paragraph::new(counter_text).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(counter_para, footer_chunks[0]);

        let shortcuts_text =
            " ↑↓/browse  ENTER/view raw  e/edit  d/delete  r/rewind  c/copy  ESC/close";
        let shortcuts_para =
            Paragraph::new(shortcuts_text).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(shortcuts_para, footer_chunks[1]);
    }
}

pub(crate) fn session_picker_highlight_index(selected: usize, has_error: bool) -> usize {
    if has_error {
        selected + 1
    } else {
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_picker_highlight_index() {
        // - error: Some, selected: 0 → highlighted display row is "✦ New session" (display index 1), NOT the ⚠ error row (index 0).
        assert_eq!(session_picker_highlight_index(0, true), 1);
        // - error: Some, selected: 1 → highlighted display row is the first session (display index 2).
        assert_eq!(session_picker_highlight_index(1, true), 2);
        // - error: None, selected: 0 → highlighted row is "New session" (display index 0).
        assert_eq!(session_picker_highlight_index(0, false), 0);
    }
}
