use super::*;
use crate::tui::event_source::{CrosstermEventSource, EventSource};
use crate::tui::terminal::{cleanup_terminal_state, PanicTerminalHookGuard};
use crate::tui::types::{App, TranscriptEntry, TuiEvent, SPINNER_FRAMES, TICK_RATE};

impl Tui {
    #[cfg(test)]
    pub(super) fn queue_pending_message(&mut self, text: String) {
        self.app.pending_message = Some(crate::tui::types::PendingMessage {
            text: text.clone(),
            attachments: vec![],
            attachment_dir: None,
        });
        // Also set the input text so it remains visible (new behavior)
        self.set_input_text(&text);
        self.refresh_input_chrome();
    }

    #[cfg(test)]
    pub(super) fn apply_draft_edit_for_test(&mut self, key: KeyEvent) {
        if let Some(pending) = self.app.pending_message.take() {
            self.app.attachments = pending.attachments;
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
                max_scroll: 0,
                streaming_assistant_idx: None,
                pending_message: None,
                completions: vec![],
                completion_index: 0,
                completion_prefix: String::new(),
                completion_suffix: String::new(),
                history: vec![],
                history_index: None,
                history_draft: String::new(),
                attachments: vec![],
                attachment_dir: None,
                paste_count: 0,
                last_known_input_width: 1,
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
        let _panic_terminal_hook = PanicTerminalHookGuard::install();
        let mut terminal = Self::setup_terminal()?;
        let mut event_source = CrosstermEventSource;
        let result = self.run_loop_inner(&mut terminal, &mut event_source).await;
        Self::restore_terminal(&mut terminal)?;
        self.config.write().exit_session()?;
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
        let mut last_tick = Instant::now();
        loop {
            terminal.draw(|frame| self.draw(frame))?;

            if self.app.should_quit || self.abort_signal.aborted_ctrld() {
                break;
            }

            let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());
            if event_source.poll(timeout)? {
                match event_source.read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key).await?
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                    }
                    Event::Paste(text) => {
                        self.handle_paste(text);
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

    fn install_external_editor_bridge(&self) {
        self.config.write().set_tui_editor_hooks(
            Some(Box::new(cleanup_terminal_state)),
            Some(Box::new(|| {
                let _ = enable_raw_mode();
                let mut stdout = io::stdout();
                let _ = stdout.execute(EnterAlternateScreen);
                if supports_keyboard_enhancement().unwrap_or(false) {
                    let _ = stdout.execute(PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                            | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
                    ));
                }
                let _ = stdout.execute(EnableMouseCapture);
                let _ = stdout.execute(EnableBracketedPaste);
                let _ = stdout.flush();
            })),
        );
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
        self.start_prompt(crate::tui::types::PendingMessage {
            text: context,
            attachments: vec![],
            attachment_dir: None,
        })
        .await
    }
}
