use crate::client::{call_chat_completions, CompletionTokenUsage};
use crate::config::{Config, GlobalConfig, Input};
use crate::hooks::{
    dispatch_hooks_with_count_and_manager, drain_async_results, inject_pending_async_context,
    AsyncHookManager, HookEvent, HookResultControl, PersistentHookManager,
};
use crate::tool::ToolResult;
use crate::ui_output::install_ui_output_sender;
use crate::utils::{create_abort_signal, AbortSignal};

/// Strip ANSI escape sequences from a string for safe display in the TUI.
fn strip_ansi(s: &str) -> String {
    // Simple state-machine strip: remove ESC [ ... m and other CSI sequences
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next(); // consume '['
                                  // consume until a letter (the command byte)
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next(); // consume ']'
                                  // OSC: consume until ST (ESC \) or BEL
                    for c in chars.by_ref() {
                        if c == '\x07' || c == '\x1b' {
                            break;
                        }
                    }
                }
                _ => {} // bare ESC — skip
            }
        } else {
            out.push(ch);
        }
    }
    out
}

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::cmp;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tui_textarea::{Input as TextInput, Key, TextArea};

const MIN_INPUT_HEIGHT: u16 = 3;
const MAX_INPUT_HEIGHT: u16 = 8;
const TICK_RATE: Duration = Duration::from_millis(80);
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Tui {
    config: GlobalConfig,
    abort_signal: AbortSignal,
    async_manager: Arc<Mutex<AsyncHookManager>>,
    persistent_manager: Arc<Mutex<PersistentHookManager>>,
    pending_async_context: Arc<Mutex<Option<String>>>,
    app: App,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    event_rx: mpsc::UnboundedReceiver<TuiEvent>,
}

struct App {
    transcript: Vec<TranscriptEntry>,
    input: TextArea<'static>,
    spinner_index: usize,
    should_quit: bool,
    llm_busy: bool,
    transcript_scroll: u16,
    /// Pending message queued to send when LLM finishes.
    /// Set when user presses Enter while llm_busy.
    /// Cleared when user edits the input (converts back to draft).
    pending_message: Option<String>,
    /// Tab-completion state
    completions: Vec<(String, Option<String>)>,
    completion_index: usize,
    /// The original text before the word being completed, used to apply completions
    completion_prefix: String,
    /// Input history (most-recent-first)
    history: Vec<String>,
    /// Index into history while navigating (None = current draft)
    history_index: Option<usize>,
    /// Saved draft text while browsing history
    history_draft: String,
}

#[derive(Clone)]
enum TranscriptEntry {
    System(String),
    User(String),
    Assistant(String),
    Error(String),
}

enum TuiEvent {
    UiOutput(String),
    Chunk(String),
    Finished {
        output: String,
        usage: CompletionTokenUsage,
        tool_results: Vec<ToolResult>,
    },
    Errored(String),
}

impl Tui {
    fn render_entry(entry: &TranscriptEntry) -> Vec<Line<'static>> {
        let (prefix, text, style) = match entry {
            TranscriptEntry::System(text) => (
                "",
                text.clone(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            ),
            TranscriptEntry::User(text) => (
                "> ",
                text.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            TranscriptEntry::Assistant(text) => ("", text.clone(), Style::default()),
            TranscriptEntry::Error(text) => (
                "error: ",
                text.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        };

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
        lines.push(Line::from(""));
        lines
    }

    async fn run_prompt_task(
        config: GlobalConfig,
        text: String,
        abort_signal: AbortSignal,
        async_manager: Arc<Mutex<AsyncHookManager>>,
        persistent_manager: Arc<Mutex<PersistentHookManager>>,
        pending_async_context: Arc<Mutex<Option<String>>>,
        event_tx: mpsc::UnboundedSender<TuiEvent>,
    ) -> Result<()> {
        let input = Input::from_str(&config, &text, None);
        Self::run_prompt_inner(
            config,
            input,
            abort_signal,
            async_manager,
            persistent_manager,
            pending_async_context,
            event_tx,
            0,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    #[async_recursion::async_recursion]
    async fn run_prompt_inner(
        config: GlobalConfig,
        mut input: Input,
        abort_signal: AbortSignal,
        async_manager: Arc<Mutex<AsyncHookManager>>,
        persistent_manager: Arc<Mutex<PersistentHookManager>>,
        pending_async_context: Arc<Mutex<Option<String>>>,
        event_tx: mpsc::UnboundedSender<TuiEvent>,
        resume_count: u32,
        with_embeddings: bool,
    ) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }
        if with_embeddings {
            input.use_embeddings(abort_signal.clone()).await?;
        }

        {
            let mut async_guard = async_manager.lock().await;
            let mut pending_guard = pending_async_context.lock().await;
            drain_async_results(&mut async_guard, &mut pending_guard);
            inject_pending_async_context(&mut input, &mut pending_guard);
        }

        let client = input.create_client()?;
        config.write().before_chat_completion(&input)?;
        let (hooks, session_id, cwd) = Self::hook_dispatch_context(&config);
        let event = HookEvent::UserPromptSubmit {
            prompt: input.text().to_string(),
        };
        {
            let async_guard = async_manager.lock().await;
            let outcome = dispatch_hooks_with_count_and_manager(
                &event,
                &hooks.entries,
                &session_id,
                &cwd,
                resume_count,
                Some(&async_guard),
                Some(&persistent_manager),
            )
            .await;
            if matches!(outcome.control, HookResultControl::Block { .. }) {
                let _ = event_tx.send(TuiEvent::Finished {
                    output: String::new(),
                    usage: Default::default(),
                    tool_results: vec![],
                });
                return Ok(());
            }
        }

        let (output, thought, tool_results, usage) = if !input.stream() {
            call_chat_completions(&input, true, false, client.as_ref(), abort_signal.clone())
                .await?
        } else {
            Self::call_chat_completions_streaming_tui(
                &input,
                client.as_ref(),
                abort_signal.clone(),
                event_tx.clone(),
            )
            .await?
        };

        config.write().after_chat_completion(
            &input,
            &output,
            thought.as_deref(),
            &tool_results,
            &usage,
        )?;

        let stop_outcome = if tool_results.is_empty() {
            let event = HookEvent::Stop {
                stop_hook_active: true,
                last_assistant_message: Some(output.clone()),
            };
            let async_guard = async_manager.lock().await;
            Some(
                dispatch_hooks_with_count_and_manager(
                    &event,
                    &hooks.entries,
                    &session_id,
                    &cwd,
                    resume_count,
                    Some(&async_guard),
                    Some(&persistent_manager),
                )
                .await,
            )
        } else {
            None
        };

        if !tool_results.is_empty() {
            let switch_agent = tool_results.iter().find_map(|v| v.switch_agent.clone());
            if let Some(switch_agent) = switch_agent {
                config.write().exit_agent()?;
                Config::use_agent(&config, &switch_agent.agent, None, abort_signal.clone()).await?;
                config.write().empty_session()?;
                let new_input = Input::from_str(&config, &switch_agent.prompt, None);
                return Self::run_prompt_inner(
                    config,
                    new_input,
                    abort_signal,
                    async_manager,
                    persistent_manager,
                    pending_async_context,
                    event_tx,
                    0,
                    true,
                )
                .await;
            }

            return Self::run_prompt_inner(
                config,
                input.merge_tool_results(output, thought, tool_results),
                abort_signal,
                async_manager,
                persistent_manager,
                pending_async_context,
                event_tx,
                resume_count,
                false,
            )
            .await;
        }

        let _ = event_tx.send(TuiEvent::Finished {
            output: output.clone(),
            usage: usage.clone(),
            tool_results: vec![],
        });

        if let Some(stop_outcome) = stop_outcome {
            let max_resume = hooks.max_resume.unwrap_or(5);
            if stop_outcome.result.resume.unwrap_or(false) && resume_count < max_resume {
                if abort_signal.aborted() {
                    return Ok(());
                }
                let context = stop_outcome
                    .result
                    .additional_context
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "Continue working on pending tasks.".to_string());
                let new_input = Input::from_str(&config, &context, None);
                return Self::run_prompt_inner(
                    config,
                    new_input,
                    abort_signal,
                    async_manager,
                    persistent_manager,
                    pending_async_context,
                    event_tx,
                    resume_count + 1,
                    true,
                )
                .await;
            }
        }

        {
            let mut async_guard = async_manager.lock().await;
            let mut pending_guard = pending_async_context.lock().await;
            let max_resume = hooks.max_resume.unwrap_or(5);
            if drain_async_results(&mut async_guard, &mut pending_guard)
                && resume_count < max_resume
            {
                if abort_signal.aborted() {
                    return Ok(());
                }
                let context = pending_guard
                    .take()
                    .filter(|value: &String| !value.is_empty())
                    .unwrap_or_else(|| "Continue working on pending tasks.".to_string());
                drop(pending_guard);
                drop(async_guard);
                let new_input = Input::from_str(&config, &context, None);
                return Self::run_prompt_inner(
                    config,
                    new_input,
                    abort_signal,
                    async_manager,
                    persistent_manager,
                    pending_async_context,
                    event_tx,
                    resume_count + 1,
                    true,
                )
                .await;
            }
        }

        Config::maybe_autoname_session(config.clone());
        Config::maybe_compress_session(config.clone());
        Ok(())
    }

    async fn call_chat_completions_streaming_tui(
        input: &Input,
        client: &dyn crate::client::Client,
        abort_signal: AbortSignal,
        event_tx: mpsc::UnboundedSender<TuiEvent>,
    ) -> Result<(
        String,
        Option<String>,
        Vec<ToolResult>,
        CompletionTokenUsage,
    )> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut handler = crate::client::SseHandler::new(tx, abort_signal.clone());

        let sender = tokio::spawn(async move {
            while let Some(evt) = rx.recv().await {
                match evt {
                    crate::client::SseEvent::Text(text) => {
                        let _ = event_tx.send(TuiEvent::Chunk(text));
                    }
                    crate::client::SseEvent::Done => break,
                }
            }
        });

        let send_ret = client.chat_completions_streaming(input, &mut handler).await;
        let aborted = handler.abort().aborted();
        let (text, thought, tool_calls, usage) = handler.take();
        let _ = sender.await;

        if aborted {
            return Ok((text, thought, vec![], usage));
        }

        match send_ret {
            Ok(_) => Ok((
                text,
                thought,
                crate::tool::eval_tool_calls(client.global_config(), tool_calls)?,
                usage,
            )),
            Err(err) => {
                if text.is_empty() {
                    Err(err)
                } else {
                    Ok((text, thought, vec![], usage))
                }
            }
        }
    }

    fn hook_dispatch_context(
        config: &GlobalConfig,
    ) -> (crate::hooks::HooksConfig, String, std::path::PathBuf) {
        let config = config.read();
        (
            config.resolved_hooks(),
            config
                .session
                .as_ref()
                .map(|session| session.name())
                .unwrap_or("default")
                .to_string(),
            std::env::current_dir().unwrap_or_default(),
        )
    }

    #[cfg(test)]
    fn queue_pending_message(&mut self, text: String) {
        self.app.pending_message = Some(text);
        self.refresh_input_chrome();
    }

    #[cfg(test)]
    fn apply_draft_edit_for_test(&mut self, key: KeyEvent) {
        if self.app.pending_message.is_some() {
            self.app.pending_message = None;
        }
        self.app.input.input(TextInput::from(key));
        self.refresh_input_chrome();
    }

    pub fn init(
        config: &GlobalConfig,
        async_manager: AsyncHookManager,
        persistent_manager: Arc<Mutex<PersistentHookManager>>,
    ) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let ui_output_tx = event_tx.clone();
        let (bridge_tx, mut bridge_rx) = mpsc::unbounded_channel::<String>();
        install_ui_output_sender(bridge_tx);
        tokio::spawn(async move {
            while let Some(text) = bridge_rx.recv().await {
                let _ = ui_output_tx.send(TuiEvent::UiOutput(text));
            }
        });

        // Build the initial transcript: welcome (if not in agent/RAG) + banner (if agent)
        let initial_transcript = Self::build_initial_transcript(config);

        Ok(Self {
            config: config.clone(),
            abort_signal: create_abort_signal(),
            async_manager: Arc::new(Mutex::new(async_manager)),
            persistent_manager,
            pending_async_context: Arc::new(Mutex::new(None)),
            app: App {
                transcript: initial_transcript,
                input: Self::new_input(),
                spinner_index: 0,
                should_quit: false,
                llm_busy: false,
                transcript_scroll: 0,
                pending_message: None,
                completions: vec![],
                completion_index: 0,
                completion_prefix: String::new(),
                history: vec![],
                history_index: None,
                history_draft: String::new(),
            },
            event_tx,
            event_rx,
        })
    }

    fn build_initial_transcript(config: &GlobalConfig) -> Vec<TranscriptEntry> {
        let mut entries = vec![];
        let cfg = config.read();
        let state = cfg.state();

        // Show welcome only when not already in an agent/RAG session (matches old REPL behaviour)
        entries.push(TranscriptEntry::System(format!(
            "Welcome to {} {}  •  Type .help for commands, Tab to complete.",
            env!("CARGO_CRATE_NAME"),
            env!("CARGO_PKG_VERSION")
        )));

        // Show agent banner and conversation starters if an agent is active
        if state.contains(crate::config::StateFlags::AGENT) {
            if let Ok(banner) = cfg.agent_banner() {
                if !banner.trim().is_empty() {
                    entries.push(TranscriptEntry::Assistant(banner));
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
                    entries.push(TranscriptEntry::System(format!(
                        "Conversation starters:\n{list}\n(type .starter <n> to use)"
                    )));
                }
            }
        }

        // Show status line if set (session / model info)
        let status = cfg.render_status_line(true);
        if !status.is_empty() {
            entries.push(TranscriptEntry::System(status));
        }

        entries
    }

    pub fn async_manager(&self) -> &Arc<Mutex<AsyncHookManager>> {
        &self.async_manager
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut terminal = Self::setup_terminal()?;
        let result = self.run_loop(&mut terminal).await;
        Self::restore_terminal(&mut terminal)?;
        self.config.write().exit_session()?;
        result
    }

    async fn run_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let mut last_tick = Instant::now();
        loop {
            terminal.draw(|frame| self.draw(frame))?;

            if self.app.should_quit || self.abort_signal.aborted_ctrld() {
                break;
            }

            let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key).await?
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }

            while let Ok(evt) = self.event_rx.try_recv() {
                self.handle_tui_event(evt).await?;
            }

            if last_tick.elapsed() >= TICK_RATE {
                self.app.spinner_index = (self.app.spinner_index + 1) % SPINNER_FRAMES.len();
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
        self.app.transcript.push(TranscriptEntry::System(format!(
            "↩ Async resume: {context}"
        )));
        self.pin_transcript_to_bottom();
        self.start_prompt(context).await
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let size = frame.area();
        let input_height = self
            .input_height()
            .clamp(MIN_INPUT_HEIGHT, MAX_INPUT_HEIGHT);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(input_height)])
            .split(size);

        let transcript_lines = if self.app.transcript.is_empty() {
            vec![Line::from(Span::raw(""))]
        } else {
            self.app
                .transcript
                .iter()
                .flat_map(Self::render_entry)
                .collect::<Vec<_>>()
        };

        // Clamp transcript_scroll to valid range (u16::MAX is the "pinned to bottom" sentinel)
        let total_lines = transcript_lines.len() as u16;
        let visible_height = chunks[0].height;
        let max_scroll = total_lines.saturating_sub(visible_height);
        if self.app.transcript_scroll > max_scroll {
            self.app.transcript_scroll = max_scroll;
        }

        let transcript = Paragraph::new(transcript_lines)
            .block(Block::default().borders(Borders::NONE).title("Transcript"))
            .wrap(Wrap { trim: false })
            .scroll((self.app.transcript_scroll, 0));
        frame.render_widget(transcript, chunks[0]);

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

    fn input_height(&self) -> u16 {
        let lines = self.app.input.lines();
        let body_lines = cmp::max(1, lines.len()) as u16;
        body_lines + 2
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.abort_signal.set_ctrld();
                self.app.should_quit = true;
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // Abort current operation; reset signal so next submit works (fix #3)
                self.abort_signal.set_ctrlc();
                self.app.transcript.push(TranscriptEntry::System(
                    "(Ctrl+C — operation aborted. Ctrl+D to exit.)".to_string(),
                ));
                self.app.llm_busy = false;
                self.abort_signal.reset();
            }
            (KeyCode::Up, KeyModifiers::NONE) => {
                if self.app.completions.is_empty() {
                    self.history_prev();
                } else {
                    self.app.transcript_scroll = self.app.transcript_scroll.saturating_sub(1);
                }
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                if self.app.completions.is_empty() {
                    self.history_next();
                } else {
                    self.app.transcript_scroll = self.app.transcript_scroll.saturating_add(1);
                }
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                self.app.transcript_scroll = self.app.transcript_scroll.saturating_sub(10);
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                self.app.transcript_scroll = self.app.transcript_scroll.saturating_add(10);
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.handle_tab(false);
            }
            (KeyCode::BackTab, KeyModifiers::SHIFT) => {
                self.handle_tab(true);
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                if !self.app.completions.is_empty() {
                    self.app.completions.clear();
                }
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.app.completions.clear();
                let text = self.app.input.lines().join("\n");
                if !text.trim().is_empty() {
                    // Reset abort signal before each new submission (fix #3)
                    self.abort_signal.reset();
                    // Add to history (fix #4)
                    self.push_history(text.clone());
                    if self.app.llm_busy {
                        // Queue the message to send when LLM finishes
                        self.app.pending_message = Some(text);
                        self.app.input = Self::new_input();
                        self.refresh_input_chrome();
                    } else if text.trim_start().starts_with('.') {
                        // Dot-command: route through repl command handler
                        self.app
                            .transcript
                            .push(TranscriptEntry::User(text.clone()));
                        self.app.input = Self::new_input();
                        self.run_repl_command(&text).await?;
                        self.refresh_input_chrome();
                    } else {
                        self.app
                            .transcript
                            .push(TranscriptEntry::User(text.clone()));
                        self.app.input = Self::new_input();
                        self.start_prompt(text).await?;
                    }
                }
            }
            (KeyCode::Enter, KeyModifiers::SHIFT) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                // Shift+Enter / Ctrl+J inserts a newline - clear pending if any
                if self.app.pending_message.is_some() {
                    self.app.pending_message = None;
                    self.refresh_input_chrome();
                }
                self.app.input.input(TextInput {
                    key: Key::Enter,
                    ..Default::default()
                });
            }
            _ => {
                // Any other key input clears pending message (converts back to draft)
                if self.app.pending_message.is_some() {
                    self.app.pending_message = None;
                    self.refresh_input_chrome();
                }
                // Clear completions on any non-tab key
                if !self.app.completions.is_empty() {
                    self.app.completions.clear();
                }
                self.app.input.input(TextInput::from(key));
            }
        }
        Ok(())
    }

    fn pin_transcript_to_bottom(&mut self) {
        self.app.transcript_scroll = u16::MAX;
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.app.transcript_scroll = self.app.transcript_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                self.app.transcript_scroll = self.app.transcript_scroll.saturating_add(3);
            }
            _ => {}
        }
    }

    async fn handle_tui_event(&mut self, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::UiOutput(text) => {
                // Strip ANSI escape codes so raw terminal output doesn't corrupt the TUI (fix #6)
                let clean = strip_ansi(&text);
                let clean = clean.trim_end_matches('\n').to_string();
                if !clean.is_empty() {
                    self.app.transcript.push(TranscriptEntry::System(clean));
                    self.pin_transcript_to_bottom();
                }
            }
            TuiEvent::Chunk(chunk) => {
                match self.app.transcript.last_mut() {
                    Some(TranscriptEntry::Assistant(existing)) => existing.push_str(&chunk),
                    _ => self.app.transcript.push(TranscriptEntry::Assistant(chunk)),
                }
                self.pin_transcript_to_bottom();
            }
            TuiEvent::Finished {
                output,
                usage,
                tool_results,
            } => {
                self.app.llm_busy = false;
                if !output.is_empty() {
                    match self.app.transcript.last_mut() {
                        Some(TranscriptEntry::Assistant(existing)) if !existing.is_empty() => {
                            if existing != &output {
                                *existing = output;
                            }
                        }
                        _ => self.app.transcript.push(TranscriptEntry::Assistant(output)),
                    }
                    self.pin_transcript_to_bottom();
                }
                if !tool_results.is_empty() {
                    self.app.transcript.push(TranscriptEntry::System(format!(
                        "{} tool result(s) returned",
                        tool_results.len()
                    )));
                    self.pin_transcript_to_bottom();
                }
                if !usage.is_empty() {
                    self.app
                        .transcript
                        .push(TranscriptEntry::System(format!("Usage: {usage}")));
                    self.pin_transcript_to_bottom();
                }
                self.refresh_input_chrome();

                if let Some(pending) = self.app.pending_message.take() {
                    self.app
                        .transcript
                        .push(TranscriptEntry::User(pending.clone()));
                    self.pin_transcript_to_bottom();
                    self.start_prompt(pending).await?;
                }
            }
            TuiEvent::Errored(err) => {
                self.app.llm_busy = false;
                self.app.transcript.push(TranscriptEntry::Error(err));
                self.pin_transcript_to_bottom();
                self.refresh_input_chrome();

                if let Some(pending) = self.app.pending_message.take() {
                    self.app
                        .transcript
                        .push(TranscriptEntry::User(pending.clone()));
                    self.pin_transcript_to_bottom();
                    self.start_prompt(pending).await?;
                }
            }
        }
        Ok(())
    }

    async fn start_prompt(&mut self, text: String) -> Result<()> {
        self.app.llm_busy = true;

        let config = self.config.clone();
        let abort_signal = self.abort_signal.clone();
        let async_manager = self.async_manager.clone();
        let persistent_manager = self.persistent_manager.clone();
        let pending_async_context = self.pending_async_context.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let result: Result<()> = Self::run_prompt_task(
                config,
                text,
                abort_signal,
                async_manager,
                persistent_manager,
                pending_async_context,
                event_tx.clone(),
            )
            .await;
            if let Err(err) = result {
                let _ = event_tx.send(TuiEvent::Errored(err.to_string()));
            }
        });

        Ok(())
    }

    fn build_input_title(&self) -> Line<'static> {
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
                    parts.push(format!("💬 {}({:.0}%)", tokens, percent));
                } else {
                    parts.push(format!("💬 {}", tokens));
                }
            }
        }

        if self.app.pending_message.is_some() {
            parts.push("Pending message queued".to_string());
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

    fn refresh_input_chrome(&mut self) {}

    fn refresh_input_chrome_from_state(
        _config: &GlobalConfig,
        _app: &mut App,
        _llm_busy: bool,
        _pending_message: bool,
    ) {
    }

    fn push_history(&mut self, text: String) {
        // Avoid duplicate of last entry
        if self.app.history.first().map(|s| s.as_str()) != Some(text.as_str()) {
            self.app.history.insert(0, text);
            // Cap history at 500 entries
            self.app.history.truncate(500);
        }
        self.app.history_index = None;
        self.app.history_draft = String::new();
    }

    fn history_prev(&mut self) {
        if self.app.history.is_empty() {
            return;
        }
        let next_index = match self.app.history_index {
            None => {
                // Save current draft before starting navigation
                self.app.history_draft = self.app.input.lines().join("\n");
                0
            }
            Some(i) if i + 1 < self.app.history.len() => i + 1,
            Some(i) => i, // Already at oldest
        };
        self.app.history_index = Some(next_index);
        let text = self.app.history[next_index].clone();
        self.set_input_text(&text);
    }

    fn history_next(&mut self) {
        match self.app.history_index {
            None => {} // Not in history navigation
            Some(0) => {
                // Back to draft
                self.app.history_index = None;
                let draft = self.app.history_draft.clone();
                self.set_input_text(&draft);
            }
            Some(i) => {
                let next = i - 1;
                self.app.history_index = Some(next);
                let text = self.app.history[next].clone();
                self.set_input_text(&text);
            }
        }
    }

    fn set_input_text(&mut self, text: &str) {
        self.app.input = Self::new_input();
        for ch in text.chars() {
            if ch == '\n' {
                self.app.input.input(TextInput {
                    key: Key::Enter,
                    ..Default::default()
                });
            } else {
                self.app.input.input(TextInput {
                    key: Key::Char(ch),
                    ..Default::default()
                });
            }
        }
    }

    fn handle_tab(&mut self, reverse: bool) {
        if !self.app.completions.is_empty() {
            // Cycle through existing completions
            if reverse {
                if self.app.completion_index == 0 {
                    self.app.completion_index = self.app.completions.len() - 1;
                } else {
                    self.app.completion_index -= 1;
                }
            } else {
                self.app.completion_index =
                    (self.app.completion_index + 1) % self.app.completions.len();
            }
            // Apply selected completion
            self.apply_completion();
            return;
        }

        // Compute new completions
        let line = self.app.input.lines().join("\n");
        let pos = {
            let cursor = self.app.input.cursor();
            // cursor is (row, col) in character offsets; convert to a byte position
            let lines = self.app.input.lines();
            let mut p = 0;
            for (i, l) in lines.iter().enumerate() {
                if i == cursor.0 {
                    let col = cursor.1.min(l.chars().count());
                    p += l
                        .char_indices()
                        .nth(col)
                        .map(|(idx, _)| idx)
                        .unwrap_or_else(|| l.len());
                    break;
                }
                p += l.len() + 1; // +1 for newline
            }
            p.min(line.len())
        };

        let completions = self.compute_completions(&line, pos);
        if completions.is_empty() {
            return;
        }

        // Compute prefix: everything before the word being completed
        let text_before = &line[..pos];
        let word_start = text_before
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        self.app.completion_prefix = line[..word_start].to_string();

        self.app.completions = completions;
        self.app.completion_index = 0;
        self.apply_completion();
    }

    fn apply_completion(&mut self) {
        if self.app.completions.is_empty() {
            return;
        }
        let (value, _) = &self.app.completions[self.app.completion_index];
        let new_text = format!("{}{}", self.app.completion_prefix, value);

        self.set_input_text(&new_text);
    }

    fn compute_completions(&self, line: &str, pos: usize) -> Vec<(String, Option<String>)> {
        let line = &line[..pos];

        // Split into parts for analysis
        let mut parts: Vec<(&str, usize)> = vec![];
        let mut part_start = None;
        for (i, ch) in line.char_indices() {
            if ch == ' ' {
                if let Some(s) = part_start {
                    parts.push((&line[s..i], s));
                    part_start = None;
                }
            } else if part_start.is_none() {
                part_start = Some(i);
            }
        }
        if let Some(s) = part_start {
            parts.push((&line[s..], s));
        } else {
            parts.push(("", line.len()));
        }

        if parts.is_empty() {
            return vec![];
        }

        let (cmd, _cmd_start) = parts[0];

        // If we're still typing the first word starting with '.', complete commands
        if parts.len() == 1 && cmd.starts_with('.') {
            let filter = cmd;
            let commands: Vec<(String, Option<String>)> = crate::repl::REPL_COMMANDS
                .iter()
                .filter(|c| c.name.starts_with(filter))
                .map(|c| (c.name.to_string(), Some(c.description.to_string())))
                .collect();
            return commands;
        }

        // For multi-part commands, delegate to config's repl_complete
        if cmd.starts_with('.') {
            let mut args: Vec<&str> = parts[1..].iter().map(|p| p.0).collect();
            if line.ends_with(' ') {
                args.push("");
            }
            let filter = args.last().copied().unwrap_or("");
            return self.config.read().repl_complete(cmd, &args, filter);
        }

        vec![]
    }

    async fn run_repl_command(&mut self, line: &str) -> Result<()> {
        let config = self.config.clone();
        let abort_signal = self.abort_signal.clone();
        let mut async_manager = self.async_manager.lock().await;
        let mut pending_async_context = self.pending_async_context.lock().await;
        match crate::repl::run_repl_command(
            &config,
            abort_signal,
            line,
            &mut async_manager,
            &self.persistent_manager,
            &mut pending_async_context,
        )
        .await
        {
            Ok(exit) => {
                if exit {
                    self.app.should_quit = true;
                }
                let llm_busy = self.app.llm_busy;
                let pending_message = self.app.pending_message.is_some();
                Self::refresh_input_chrome_from_state(
                    &self.config,
                    &mut self.app,
                    llm_busy,
                    pending_message,
                );
            }
            Err(err) => {
                self.app
                    .transcript
                    .push(TranscriptEntry::Error(err.to_string()));
            }
        }
        Ok(())
    }

    fn new_input() -> TextArea<'static> {
        let mut input = TextArea::default();
        input.set_block(
            Block::default()
                .borders(Borders::TOP)
                .title("Input")
                .border_style(Style::default()),
        );
        input.set_cursor_line_style(Style::default());
        // input.set_placeholder_text("Enter submits · Shift+Enter / Ctrl+J for newline");
        input
    }

    fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        stdout.execute(EnterAlternateScreen)?;
        stdout.execute(EnableMouseCapture)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(terminal)
    }

    fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        disable_raw_mode()?;
        terminal.backend_mut().execute(DisableMouseCapture)?;
        terminal.backend_mut().execute(LeaveAlternateScreen)?;
        terminal.show_cursor()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock;

    fn test_config() -> GlobalConfig {
        Arc::new(RwLock::new(Config::default()))
    }

    #[tokio::test]
    async fn pending_message_is_cleared_when_user_edits_again() {
        let config = test_config();
        let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
        let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
        tui.app.llm_busy = true;
        tui.queue_pending_message("queued message".to_string());

        tui.apply_draft_edit_for_test(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        assert!(tui.app.pending_message.is_none());
        assert_eq!(tui.app.input.lines().join("\n"), "x");
    }

    #[tokio::test]
    async fn pending_message_is_auto_sent_after_finish() {
        let config = test_config();
        let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
        let mut tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();
        tui.app.llm_busy = true;
        tui.queue_pending_message("follow up".to_string());

        tui.handle_tui_event(TuiEvent::Finished {
            output: "done".to_string(),
            usage: Default::default(),
            tool_results: vec![],
        })
        .await
        .unwrap();

        assert!(tui.app.llm_busy);
        assert!(tui.app.pending_message.is_none());
        let has_user_entry = tui
            .app
            .transcript
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::User(text) if text == "follow up"));
        assert!(has_user_entry);
    }

    #[tokio::test]
    async fn compute_completions_handles_trailing_space_after_command() {
        let config = test_config();
        let persistent = Arc::new(Mutex::new(PersistentHookManager::new()));
        let tui = Tui::init(&config, AsyncHookManager::new(), persistent).unwrap();

        let line = ".model ";
        let _completions = tui.compute_completions(line, line.len());
    }
}
