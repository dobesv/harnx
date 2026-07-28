use crate::agent_event_sink::install_tui_agent_event_sink;
use crate::event_source::{CrosstermEventSource, EventSource};
use crate::terminal::{cleanup_terminal_state, PanicTerminalHookGuard};
use crate::types::Tui;
use crate::types::{
    App, ModalState, PendingMessage, TranscriptItem, TuiEvent, SPINNER_FRAMES, TICK_RATE,
};
use anyhow::Result;
#[cfg(test)]
use crossterm::event::KeyEvent;
use crossterm::event::{
    EnableBracketedPaste, EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen};
use crossterm::ExecutableCommand;
use harnx_core::message::Message;
use harnx_hooks::{drain_async_results, AsyncHookManager, PersistentHookManager};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::config::{
    build_picker_context, list_assistant_agents, sort_sessions_for_picker,
};
use harnx_runtime::tool::ToolDeclaration;
use harnx_runtime::utils::create_abort_signal;
use log::warn;
use ratatui::Terminal;
#[cfg(test)]
use ratatui_textarea::Input as TextInput;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

/// How often the memory watchdog samples RSS. Cheap; only logs on threshold
/// crossings.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);
/// First RSS threshold (bytes) the watchdog warns at; it then doubles on each
/// crossing. 512 MiB sits above normal sessions so quiet runs log nothing.
const WATCHDOG_RSS_FLOOR: u64 = 512 * 1024 * 1024;
/// Draining at least this many events from `event_rx` in a single tick is
/// treated as a producer flooding the channel and is logged.
const WATCHDOG_EVENT_DRAIN: usize = 2_000;

/// Best-effort resident-set size of this process in bytes (Linux only).
/// Returns `None` on other platforms or if `/proc` is unreadable. Used purely
/// by the memory watchdog to localize the #842 OOM: if RSS climbs while the
/// transcript stays small, the leak is outside the transcript.
#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    // /proc/self/statm field 2 (0-indexed 1) is resident pages. Page size is
    // assumed 4 KiB — fine for an order-of-magnitude diagnostic.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident_pages.saturating_mul(4096))
}

#[cfg(not(target_os = "linux"))]
fn process_rss_bytes() -> Option<u64> {
    None
}

/// (item count, summed text bytes) for the transcript — a coarse proxy for how
/// much of the process's memory lives in transcript items. Ignores cached
/// renders and tool-call bodies; the text fields dominate.
fn transcript_footprint(items: &[TranscriptItem]) -> (usize, usize) {
    let bytes = items
        .iter()
        .map(|item| match item {
            TranscriptItem::SystemText(s)
            | TranscriptItem::ErrorText(s)
            | TranscriptItem::ThoughtText(s)
            | TranscriptItem::StatusLine(s)
            | TranscriptItem::UsageLine(s)
            | TranscriptItem::AttachmentHeader(s)
            | TranscriptItem::AttachmentItem(s)
            | TranscriptItem::AttachmentPreviewLine(s)
            | TranscriptItem::MutationNotice(s) => s.len(),
            TranscriptItem::UserText { text, .. }
            | TranscriptItem::AssistantText { text, .. }
            | TranscriptItem::ToolResultMarkdown { text, .. } => text.len(),
            TranscriptItem::CompactionMarker {
                text,
                summary_text,
                detail_text,
                ..
            } => text.len() + summary_text.len() + detail_text.len(),
            TranscriptItem::ToolCall { tool_name, .. } => tool_name.len(),
            TranscriptItem::Plan(_) | TranscriptItem::SourceHeading(_) => 0,
        })
        .sum();
    (items.len(), bytes)
}

fn build_initial_app(
    config: &GlobalConfig,
    initial_transcript: Vec<TranscriptItem>,
) -> Result<App> {
    Ok(App {
        transcript: initial_transcript,
        input: Tui::new_input(),
        spinner_index: 0,
        should_quit: false,
        llm_busy: false,
        scroll_state: ratatui_widget_scrolling::ScrollState::new(),
        streaming_open: false,
        streamed_text_this_turn: false,
        cache_valid_width: None,
        last_ui_output_source: None,
        last_usage_source: None,
        last_usage_transcript_idx: None,
        pending_thought_source: None,
        pending_thought_text: String::new(),
        pending_tool_seq: None,
        pending_message: None,
        completions: vec![],
        completion_index: 0,
        completion_prefix: String::new(),
        completion_suffix: String::new(),
        history: vec![],
        history_index: None,
        history_draft: String::new(),
        history_preview: false,
        attachments: vec![],
        attachment_dir: None,
        paste_count: 0,
        last_known_input_width: 1,
        show_sequence_numbers: config.read().show_sequence_numbers,
        show_timestamps: config.read().show_timestamps,
        transcript_focus: None,
        transcript_selection_anchor: None,
        modal: None,
        pending_confirm_reply: None,
        detail_view_scroll: {
            let mut s = ratatui_widget_scrolling::ScrollState::new();
            s.follow = false;
            s
        },
        detail_view_open: false,
        detail_view_raw_yaml: None,
        detail_view_text: None,
        detail_view_title: None,
        transcript_browsing: false,
        browsing_view_scroll: {
            let mut s = ratatui_widget_scrolling::ScrollState::new();
            s.follow = false;
            s
        },
        copy_notice_until: None,
        scroll_to_focused_item: false,
        use_utc_timestamps: false,
    })
}

impl Tui {
    #[cfg(test)]
    pub(super) async fn queue_pending_message(&mut self, text: String) {
        let pending = PendingMessage {
            text: text.clone(),
            attachments: vec![],
            attachment_dir: None,
            paste_count: self.app.paste_count,
        };
        self.app.pending_message = Some(pending.clone());
        // Also publish to the shared state so the prompt task can consume it.
        *self.shared_pending_message.lock().await = Some(pending);
        // Also set the input text so it remains visible (new behavior)
        self.set_input_text(&text);
        self.refresh_input_chrome();
    }

    #[cfg(test)]
    pub(super) fn apply_draft_edit_for_test(&mut self, key: KeyEvent) {
        if let Some(pending) = self.app.pending_message.take() {
            self.app.attachments = pending.attachments;
            self.app.paste_count = pending.paste_count;
        }
        self.app.input.input(TextInput::from(key));
        self.refresh_input_chrome();
    }

    /// Check whether agents are available to start the TUI.
    /// Returns an error if no agent is configured and no assistant agents are
    /// installed on disk. Extracted so it can be called and tested independently
    /// of the full `init()` path.
    pub(crate) fn check_agents_available(config: &GlobalConfig, agents: &[String]) -> Result<()> {
        if config.read().agent.is_none() && agents.is_empty() {
            anyhow::bail!(
                "No agents configured. Create an agent file in the agents/ directory first."
            );
        }
        Ok(())
    }

    pub async fn init(
        config: &GlobalConfig,
        async_manager: AsyncHookManager,
        persistent_manager: Arc<Mutex<PersistentHookManager>>,
    ) -> Result<Self> {
        let agents = list_assistant_agents().await;
        // Skip agents check in tests: test configs use a bare `Config::default()`
        // with no agents directory populated, but the production check is covered
        // by direct unit tests of `check_agents_available`.
        if !cfg!(test) {
            Self::check_agents_available(config, &agents)?;
        }

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        install_tui_agent_event_sink(event_tx.clone());

        // Build the initial transcript: welcome + banner (if agent)
        let initial_transcript = Self::build_initial_transcript(config);
        let code_theme = config.read().render_options()?.theme;

        let mut app = build_initial_app(config, initial_transcript)?;

        if !cfg!(test) {
            app.modal = Self::resolve_initial_modal(config).await;
        }

        Ok(Self {
            config: config.clone(),
            code_theme,
            abort_signal: create_abort_signal(),
            async_manager: Arc::new(Mutex::new(async_manager)),
            persistent_manager,
            pending_async_context: Arc::new(Mutex::new(None)),
            shared_pending_message: Arc::new(Mutex::new(None)),
            local_worker: Arc::new(Mutex::new(None)),
            current_prompt_abort: None,
            current_prompt_handle: None,
            active_remote_session: None,
            needs_full_redraw: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            app,
            event_tx,
            event_rx,
        })
    }

    pub(crate) async fn resolve_initial_modal(config: &GlobalConfig) -> Option<ModalState> {
        let agents = list_assistant_agents().await;
        if config.read().agent.is_none() {
            if agents.is_empty() {
                return None;
            }
            return Some(ModalState::AgentPicker {
                agents,
                selected: 0,
                query: String::new(),
            });
        }
        if config.read().session.is_none() {
            // sessions_dir() is already scoped to the active agent, so no
            // extra agent_name filter is needed here.
            let cluster = config
                .read()
                .remote_agent
                .as_ref()
                .map(|(_, cluster)| cluster.clone())
                .unwrap_or_else(|| harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string());

            let cfg = config.read().clone();
            let (sessions, fetch_error) = match cfg.list_remote_sessions_with_meta(&cluster).await {
                Ok(sessions) => (sessions, None),
                Err(error) => {
                    log::warn!(
                        "Failed to list NATS sessions for cluster '{}': {:#}",
                        cluster,
                        error
                    );
                    (
                        vec![],
                        Some(format!("NATS sessions unavailable: {error:#}")),
                    )
                }
            };

            let ctx = build_picker_context(None);
            let sorted = sort_sessions_for_picker(sessions, &ctx);
            // Always show picker when agent active but no session — even empty list
            // because picker now always has a "New session" first item
            let origin_agent = config.read().agent.as_ref().map(|a| a.name().to_string());
            let origin_session = config.read().session.as_ref().map(|s| s.id().to_string());
            return Some(ModalState::SessionPicker {
                sessions: sorted,
                selected: 0,
                origin_agent,
                origin_session,
                error: fetch_error,
            });
        }
        None
    }

    pub(crate) fn build_initial_transcript(config: &GlobalConfig) -> Vec<TranscriptItem> {
        let mut entries = vec![];
        let cfg = config.read();
        let state = cfg.state();

        // Show the welcome banner on startup, even when an agent/session status line is also present.
        entries.push(TranscriptItem::SystemText(format!(
            "Welcome to harnx {}  •  Type .help for commands, Tab to complete.",
            env!("CARGO_PKG_VERSION")
        )));

        let history = session_history_transcript_items(config);
        if !history.is_empty() {
            entries.extend(history);
        } else {
            // Show agent banner and conversation starters if an agent is active
            if state.contains(harnx_runtime::config::StateFlags::AGENT) {
                if let Ok(banner) = cfg.agent_banner() {
                    if !banner.trim().is_empty() {
                        entries.push(TranscriptItem::AssistantText {
                            text: banner,
                            seq: None,
                            timestamp: None, // Banner has no timestamp
                            rendered_cache: None,
                        });
                    }
                }
                if let Some(agent) = &cfg.agent {
                    let starters = agent.conversation_staters();
                    if !starters.is_empty() {
                        let list = starters
                            .iter()
                            .enumerate()
                            .map(|(i, s)| format!("  {}. {s}", i + 1))
                            .collect::<Vec<_>>()
                            .join("\n");
                        entries.push(TranscriptItem::SystemText(format!(
                            "Conversation starters:\n{list}\n(type .starter <n> to use)"
                        )));
                    }
                }
            }
        }

        // Show status line if set (session / model info)
        let status = cfg.render_status_line(true);
        if !status.is_empty() {
            entries.push(TranscriptItem::SystemText(status));
        }

        entries
    }

    pub fn async_manager(&self) -> &Arc<Mutex<AsyncHookManager>> {
        &self.async_manager
    }

    /// Returns a reference to the transcript items collected during the session.
    pub fn transcript(&self) -> &[TranscriptItem] {
        &self.app.transcript
    }

    pub async fn run(&mut self) -> Result<()> {
        let _panic_terminal_hook = PanicTerminalHookGuard::install();
        let mut terminal = Self::setup_terminal()?;

        if let Some(title) = self.config.read().session.as_ref().and_then(|s| s.title()) {
            let _ = std::io::stdout().execute(crossterm::terminal::SetTitle(title));
        }

        let mut event_source = CrosstermEventSource;
        let result = self.run_loop_inner(&mut terminal, &mut event_source).await;
        Self::restore_terminal(&mut terminal)?;
        result
    }

    /// Generic run_loop that works with any Backend and EventSource.
    /// Used by production `run()` with Crossterm, and by tests with TestBackend.
    async fn run_loop_inner<B, E>(
        &mut self,
        terminal: &mut Terminal<B>,
        event_source: &mut E,
    ) -> Result<()>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
        E: EventSource,
    {
        self.install_external_editor_bridge();
        self.install_tool_confirm_bridge();
        let mut last_tick = Instant::now();
        // Memory watchdog for the intermittent OOM (#842). Cheap in the common
        // case: once a second we read RSS and only emit a (warn-level) line
        // when it crosses the next doubling threshold, so normal sessions log
        // nothing. When it does fire it records whether the growth is in the
        // transcript / event channel (the render path) or elsewhere
        // (compaction task, streaming buffers), which is what we can't tell
        // from the OOM alone.
        let mut last_watchdog = Instant::now();
        let mut rss_warn_threshold: u64 = WATCHDOG_RSS_FLOOR;
        loop {
            // After an external editor exits, the terminal buffer is stale.
            // Clear it to force ratatui to repaint every cell from scratch.
            if self
                .needs_full_redraw
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                let _ = terminal.clear();
            }
            terminal.draw(|frame| self.draw(frame))?;

            if self.app.should_quit || self.abort_signal.aborted_ctrld() {
                break;
            }

            let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());
            if event_source.poll(timeout)? {
                match event_source.read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        self.handle_key(key).await?
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                    }
                    Event::Paste(text) => {
                        self.handle_paste(text).await;
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }

            let mut drained = 0usize;
            while let Ok(evt) = self.event_rx.try_recv() {
                self.handle_tui_event(evt).await?;
                drained += 1;
            }
            if drained >= WATCHDOG_EVENT_DRAIN {
                // A single tick draining this many events means a producer is
                // flooding the channel faster than the UI consumes it — the
                // prime suspect for unbounded transcript growth.
                warn!(
                    "tui-watchdog: drained {drained} events in one tick; \
                     event_rx_backlog={} transcript_items={} llm_busy={} compacting={}",
                    self.event_rx.len(),
                    self.app.transcript.len(),
                    self.app.llm_busy,
                    self.config.read().is_compacting_session(),
                );
            }

            if last_watchdog.elapsed() >= WATCHDOG_INTERVAL {
                last_watchdog = Instant::now();
                if let Some(rss) = process_rss_bytes() {
                    if rss >= rss_warn_threshold {
                        let (items, text_bytes) = transcript_footprint(&self.app.transcript);
                        warn!(
                            "tui-watchdog: RSS={}MiB transcript_items={items} \
                             transcript_text={}MiB event_rx_backlog={} \
                             llm_busy={} compacting={}",
                            rss / (1024 * 1024),
                            text_bytes / (1024 * 1024),
                            self.event_rx.len(),
                            self.app.llm_busy,
                            self.config.read().is_compacting_session(),
                        );
                        // Raise the bar past current RSS so we log the climb
                        // (one line per doubling) rather than every second.
                        while rss_warn_threshold <= rss {
                            rss_warn_threshold = rss_warn_threshold.saturating_mul(2);
                        }
                    }
                }
            }

            if last_tick.elapsed() >= TICK_RATE {
                self.app.spinner_index = (self.app.spinner_index + 1) % SPINNER_FRAMES.len();
                {
                    let cfg = self.config.read();
                    self.app.show_sequence_numbers = cfg.show_sequence_numbers;
                    self.app.show_timestamps = cfg.show_timestamps;
                }
                self.refresh_input_chrome();
                last_tick = Instant::now();

                // Check for async hook results that need a follow-up prompt (fix #2)
                if !self.app.llm_busy {
                    self.try_resume_async_hooks().await?;
                }
            }
        }
        Ok(())
    }

    fn install_external_editor_bridge(&self) {
        let needs_full_redraw = self.needs_full_redraw.clone();
        self.config.write().set_tui_editor_hooks(
            Some(Box::new(cleanup_terminal_state)),
            Some(Box::new(move || {
                let _ = enable_raw_mode();
                let mut stdout = io::stdout();
                let _ = stdout.execute(EnterAlternateScreen);
                if supports_keyboard_enhancement().unwrap_or(false) {
                    let _ = stdout.execute(PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
                    ));
                }
                let _ = stdout.execute(EnableMouseCapture);
                let _ = stdout.execute(EnableBracketedPaste);
                let _ = stdout.flush();
                // Signal the main loop to clear the terminal's diff buffer so
                // the next draw() repaints every cell from scratch.
                needs_full_redraw.store(true, std::sync::atomic::Ordering::Release);
            })),
        );
    }

    /// Route `PreToolUse` "ask" confirmations through the TUI instead of the
    /// default `inquire` terminal prompt (which fights ratatui's alternate
    /// screen). The callback runs on the blocked tool-eval thread: it sends a
    /// `ConfirmToolUse` event to the main loop and blocks on a reply channel
    /// until the user answers the modal. If the channel drops (e.g. the TUI is
    /// quitting), it denies.
    fn install_tool_confirm_bridge(&self) {
        let event_tx = self.event_tx.clone();
        let confirm: std::sync::Arc<harnx_runtime::tool::ConfirmToolUseFn> = std::sync::Arc::new(
            move |call: &harnx_core::tool::ToolCall,
                  input: &serde_json::Value,
                  reason: Option<&str>| {
                let (reply_tx, reply_rx) = std::sync::mpsc::channel::<bool>();
                let event = TuiEvent::ConfirmToolUse {
                    tool_name: call.name.clone(),
                    input_preview: confirm_input_preview(input),
                    reason: reason.map(str::to_string),
                    reply: reply_tx,
                };
                if event_tx.send(event).is_err() {
                    return harnx_runtime::tool::ToolUseConfirmation::Deny { reason: None };
                }
                if reply_rx.recv().unwrap_or(false) {
                    harnx_runtime::tool::ToolUseConfirmation::Approve
                } else {
                    harnx_runtime::tool::ToolUseConfirmation::Deny { reason: None }
                }
            },
        );
        self.config.write().set_tui_confirm_tool_use(Some(confirm));
    }

    /// Check if an async hook has signalled a resume and automatically start the follow-up prompt.
    async fn try_resume_async_hooks(&mut self) -> Result<()> {
        let max_resume = self.config.read().resolved_hooks().max_resume.unwrap_or(5);
        let should_resume = {
            let mut async_guard = self.async_manager.lock().await;
            let mut pending_guard = self.pending_async_context.lock().await;
            drain_async_results(&mut async_guard, &mut pending_guard)
        };
        if !should_resume {
            return Ok(());
        }
        if self.abort_signal.aborted() {
            return Ok(());
        }
        let context = {
            let mut pending_guard = self.pending_async_context.lock().await;
            pending_guard
                .take()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "Continue working on pending tasks.".to_string())
        };
        let _ = max_resume; // used inside run_prompt_inner
        self.app.transcript.push(TranscriptItem::SystemText(format!(
            "↩ Async resume: {context}"
        )));
        self.pin_transcript_to_bottom();
        self.start_prompt(PendingMessage {
            text: context,
            attachments: vec![],
            attachment_dir: None,
            paste_count: 0,
        })
        .await
    }
}

pub(crate) fn messages_to_transcript_items(
    messages: &[Message],
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Vec<TranscriptItem> {
    use harnx_core::message::{MessageContent, MessageRole};
    use serde_json::Value;

    let mut items = Vec::new();
    for msg in messages {
        match msg.role {
            MessageRole::System => {}
            MessageRole::User => {
                let text = msg.content.to_text();
                if !text.is_empty() {
                    items.push(TranscriptItem::UserText {
                        text,
                        seq: msg.log_seq,
                        timestamp: msg.log_timestamp,
                    });
                }
            }
            MessageRole::Assistant => {
                let text = msg.content.to_text();
                if !text.is_empty() {
                    items.push(TranscriptItem::AssistantText {
                        text,
                        seq: msg.log_seq,
                        timestamp: msg.log_timestamp,
                        rendered_cache: None,
                    });
                }
            }
            MessageRole::Tool => {
                if let MessageContent::ToolCalls(ref tc) = msg.content {
                    if !tc.thought.as_deref().unwrap_or("").trim().is_empty() {
                        items.push(TranscriptItem::ThoughtText(
                            tc.thought.as_deref().unwrap().trim().to_string(),
                        ));
                    }
                    if !tc.text.trim().is_empty() {
                        items.push(TranscriptItem::AssistantText {
                            text: tc.text.clone(),
                            seq: msg.log_seq,
                            timestamp: msg.log_timestamp,
                            rendered_cache: None,
                        });
                    }
                    for r in &tc.tool_results {
                        let raw_call_fallback = if r.call.arguments == Value::Null {
                            String::new()
                        } else {
                            harnx_runtime::utils::pretty_yaml_block(&r.call.arguments)
                        };
                        // Always attempt template rendering, even for zero-arg tools
                        // (raw_call_fallback may be empty but a call_template can still render).
                        let body = match harnx_runtime::tool::render_call_for_display(
                            &r.call,
                            &r.call.arguments,
                            &raw_call_fallback,
                            decl_map,
                        ) {
                            Some(rendered) => Some(crate::types::ToolCallBody::Markdown(rendered)),
                            None if !raw_call_fallback.is_empty() => {
                                Some(crate::types::ToolCallBody::Yaml(raw_call_fallback))
                            }
                            None => None,
                        };
                        items.push(TranscriptItem::ToolCall {
                            tool_name: r.call.name.clone(),
                            body,
                            seq: msg.log_seq,
                            timestamp: msg.log_timestamp,
                            rendered_cache: None,
                        });
                        let raw_result_fallback = harnx_core::tool::extract_user_display_text(
                            &r.output,
                        )
                        .unwrap_or_else(|| harnx_runtime::utils::pretty_yaml_block(&r.output));
                        let rendered_result = harnx_runtime::tool::render_result_for_display(
                            &r.call,
                            &r.output,
                            &raw_result_fallback,
                            decl_map,
                        );
                        let rendered = crate::agent_event_sink::render_tool_result_text(
                            &r.output,
                            rendered_result.as_deref(),
                        );
                        let trimmed = rendered.trim_end_matches('\n');
                        if !trimmed.is_empty() {
                            items.push(TranscriptItem::ToolResultMarkdown {
                                text: trimmed.to_string(),
                                rendered_cache: None,
                            });
                        }
                    }
                }
            }
        }
    }
    items
}

fn flatten_transcript_item_to_compaction_lines(item: &TranscriptItem, lines: &mut Vec<String>) {
    match item {
        TranscriptItem::UserText { text, .. }
        | TranscriptItem::AssistantText { text, .. }
        | TranscriptItem::SystemText(text)
        | TranscriptItem::ErrorText(text)
        | TranscriptItem::ThoughtText(text)
        | TranscriptItem::StatusLine(text)
        | TranscriptItem::UsageLine(text)
        | TranscriptItem::AttachmentHeader(text)
        | TranscriptItem::AttachmentItem(text)
        | TranscriptItem::AttachmentPreviewLine(text)
        | TranscriptItem::MutationNotice(text) => lines.extend(text.lines().map(str::to_string)),
        TranscriptItem::ToolCall {
            tool_name, body, ..
        } => {
            lines.push(format!("name: {tool_name}"));
            match body {
                Some(crate::types::ToolCallBody::Yaml(body))
                | Some(crate::types::ToolCallBody::Markdown(body)) => {
                    lines.extend(body.lines().map(str::to_string));
                }
                None => {}
            }
        }
        TranscriptItem::ToolResultMarkdown { text, .. } => {
            lines.extend(text.lines().map(str::to_string));
        }
        TranscriptItem::Plan(entries) => {
            for entry in entries {
                lines.push(format!("- [{}] {}", entry.status, entry.content));
            }
        }
        TranscriptItem::SourceHeading(source) => {
            lines.push(crate::render_helpers::source_heading(source));
        }
        TranscriptItem::CompactionMarker { text, .. } => lines.push(text.clone()),
    }
}

fn compaction_section_label(item: &TranscriptItem) -> Option<String> {
    match item {
        TranscriptItem::UserText { .. } => Some("── user ──".to_string()),
        TranscriptItem::AssistantText { .. } => Some("── assistant ──".to_string()),
        TranscriptItem::ToolCall { tool_name, .. } => Some(format!("── tool call: {tool_name} ──")),
        TranscriptItem::ToolResultMarkdown { .. } => Some("── tool result ──".to_string()),
        TranscriptItem::ThoughtText(_) => Some("── thinking ──".to_string()),
        _ => None,
    }
}

fn append_message_compaction_detail_sections(
    detail_sections: &mut Vec<String>,
    message: &Message,
    decl_map: &HashMap<String, ToolDeclaration>,
) {
    use harnx_core::message::MessageRole;

    if message.role == MessageRole::System {
        let text = message.content.to_text();
        if !text.is_empty() {
            detail_sections.push(format!(
                "── system ──
{text}"
            ));
        }
        return;
    }

    for item in messages_to_transcript_items(std::slice::from_ref(message), decl_map) {
        if let Some(label) = compaction_section_label(&item) {
            let mut section_lines = vec![label];
            flatten_transcript_item_to_compaction_lines(&item, &mut section_lines);
            detail_sections.push(section_lines.join(
                "
",
            ));
        }
    }
}

fn build_compaction_marker(
    messages: &[Message],
    summary_text: String,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> TranscriptItem {
    let from_seq = messages.iter().filter_map(|msg| msg.log_seq).min();
    let to_seq = messages.iter().filter_map(|msg| msg.log_seq).max();
    let archived_count = messages.len();
    let mut detail_sections = Vec::new();
    if !summary_text.is_empty() {
        detail_sections.push(format!("── summary ──\n{summary_text}"));
    }
    detail_sections.push(format!("archived messages: {archived_count}"));
    for message in messages {
        append_message_compaction_detail_sections(&mut detail_sections, message, decl_map);
    }
    let detail_text = detail_sections.join("\n\n");
    TranscriptItem::CompactionMarker {
        text: "─── session compacted ───".to_string(),
        summary_text,
        from_seq,
        to_seq,
        detail_text,
    }
}

/// Build the transcript for a (possibly compacted) session view.
///
/// When `compressed_messages` is non-empty the archived history is rendered
/// inline first so it stays visible, followed by a single compaction marker
/// carrying summary text, then the active/preserved messages. Shared by local
/// and remote resume paths to keep layout in lockstep (#904).
pub(crate) fn build_transcript_with_compaction(
    compressed_messages: &[Message],
    active_messages: &[Message],
    compaction_summary: Option<&str>,
    decl_map: &HashMap<String, ToolDeclaration>,
) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    if !compressed_messages.is_empty() {
        items.extend(messages_to_transcript_items(compressed_messages, decl_map));
        items.push(build_compaction_marker(
            compressed_messages,
            compaction_summary.unwrap_or_default().to_string(),
            decl_map,
        ));
    }
    items.extend(messages_to_transcript_items(active_messages, decl_map));
    items
}

pub(crate) fn session_history_transcript_items(config: &GlobalConfig) -> Vec<TranscriptItem> {
    let cfg = config.read();
    let session_id = match cfg.session.as_ref() {
        Some(s) if !s.is_empty() => s.id().to_string(),
        _ => return vec![],
    };
    let thin_agent = cfg.remote_agent.clone().or_else(|| {
        cfg.agent.as_ref().map(|agent| {
            (
                agent.name().to_string(),
                harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
            )
        })
    });
    // Spell handoff tool declaration names relative to the active agent's
    // package so the transcript matches what the agent actually saw (#709).
    let active_pkg = cfg.active_package();
    let (declarations, _handoff_targets) =
        cfg.tool_declarations_for_use_tools(Some("*"), active_pkg.as_deref());
    let decl_map: HashMap<String, ToolDeclaration> = declarations
        .into_iter()
        .map(|d| (d.name.clone(), d))
        .collect();

    #[cfg(test)]
    if cfg.remote_agent.is_none() {
        let session = cfg.session.as_ref().expect("checked above");
        return build_transcript_with_compaction(
            &session.compressed_messages,
            &session.messages,
            session.compaction_summary.as_deref(),
            &decl_map,
        );
    }

    if let Some((agent, cluster)) = thin_agent {
        drop(cfg);
        let Some(state) = crate::session_history_loader::load_remote_session_history(
            config, agent, cluster, session_id,
        ) else {
            return vec![];
        };
        return build_transcript_with_compaction(
            &state.compressed_messages,
            &state.messages,
            state.compaction_summary.as_deref(),
            &decl_map,
        );
    }

    let session = cfg.session.as_ref().expect("checked above");
    build_transcript_with_compaction(
        &session.compressed_messages,
        &session.messages,
        session.compaction_summary.as_deref(),
        &decl_map,
    )
}

/// Compact one-line preview of a tool call's arguments for the confirmation
/// modal. Renders as inline JSON and truncates to keep the modal tidy.
fn confirm_input_preview(input: &serde_json::Value) -> String {
    if input.is_null() {
        return String::new();
    }
    let rendered = serde_json::to_string(input).unwrap_or_default();
    const MAX: usize = 160;
    if rendered.chars().count() > MAX {
        let head: String = rendered.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCallBody, TranscriptItem};
    use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
    use harnx_core::tool::{ToolCall, ToolResult};
    use harnx_runtime::tool::ToolDeclaration;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn messages_to_transcript_uses_markdown_when_template_exists() {
        let mut decl_map: HashMap<String, ToolDeclaration> = HashMap::new();
        let decl = ToolDeclaration {
            name: "bash_exec".to_string(),
            description: String::new(),
            parameters: Default::default(),
            mcp_tool_name: Some("bash_exec".to_string()),
            mcp_server_name: None,
            call_template: Some("${{ args.command }}".to_string()),
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
        };
        decl_map.insert(decl.name.clone(), decl);

        let call = ToolCall::new(
            "bash_exec".to_string(),
            json!({"command": "ls -la"}),
            Some("call-1".to_string()),
            None,
        );
        let tool_result = ToolResult::new(call, json!({"output": "file.txt"}));
        let tc = MessageContentToolCalls::new(vec![tool_result], String::new(), None);
        let messages = vec![Message {
            role: MessageRole::Tool,
            content: MessageContent::ToolCalls(tc),
            id: None,
            log_seq: None,
            log_timestamp: None,
        }];

        let items = messages_to_transcript_items(&messages, &decl_map);

        let tool_call_item = items.iter().find(|item| {
            matches!(item, TranscriptItem::ToolCall { tool_name, .. } if tool_name == "bash_exec")
        });
        assert!(
            tool_call_item.is_some(),
            "expected ToolCall transcript item"
        );
        match tool_call_item.unwrap() {
            TranscriptItem::ToolCall {
                body: Some(ToolCallBody::Markdown(rendered)),
                ..
            } => {
                assert!(
                    rendered.contains("ls -la"),
                    "rendered body should contain command: got {rendered:?}"
                );
            }
            TranscriptItem::ToolCall { body, .. } => {
                panic!("expected ToolCallBody::Markdown, got {body:?}");
            }
            _ => panic!("wrong item type"),
        }
    }

    #[test]
    fn messages_to_transcript_falls_back_to_yaml_when_no_template() {
        let decl = ToolDeclaration {
            name: "no_template_tool".to_string(),
            description: String::new(),
            parameters: Default::default(),
            mcp_tool_name: Some("no_template_tool".to_string()),
            mcp_server_name: None,
            call_template: None,
            result_template: None,
            idempotent_hint: None,
            read_only_hint: None,
        };
        let mut decl_map = HashMap::new();
        decl_map.insert(decl.name.clone(), decl);

        let call = ToolCall::new(
            "no_template_tool".to_string(),
            json!({"key": "value"}),
            Some("call-2".to_string()),
            None,
        );
        let tool_result = ToolResult::new(call, json!({"output": "ok"}));
        let tc = MessageContentToolCalls::new(vec![tool_result], String::new(), None);
        let messages = vec![Message {
            role: MessageRole::Tool,
            content: MessageContent::ToolCalls(tc),
            id: None,
            log_seq: None,
            log_timestamp: None,
        }];

        let items = messages_to_transcript_items(&messages, &decl_map);

        let tool_call_item = items.iter().find(|item| {
            matches!(item, TranscriptItem::ToolCall { tool_name, .. } if tool_name == "no_template_tool")
        });
        assert!(
            tool_call_item.is_some(),
            "expected ToolCall transcript item"
        );
        match tool_call_item.unwrap() {
            TranscriptItem::ToolCall {
                body: Some(ToolCallBody::Yaml(_)),
                ..
            } => {}
            TranscriptItem::ToolCall { body, .. } => {
                panic!("expected ToolCallBody::Yaml fallback, got {body:?}");
            }
            _ => panic!("wrong item type"),
        }
    }
}
