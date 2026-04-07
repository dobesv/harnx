use super::*;
use crate::tui::types::{TranscriptEntry, TuiEvent};

impl Tui {
    pub(super) async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
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
                if self.app.transcript_scroll == u16::MAX {
                    self.app.transcript_scroll = self.app.max_scroll.saturating_sub(10);
                } else {
                    self.app.transcript_scroll = self.app.transcript_scroll.saturating_sub(10);
                }
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                let new_scroll = self.app.transcript_scroll.saturating_add(10);
                if new_scroll >= self.app.max_scroll {
                    self.app.transcript_scroll = u16::MAX;
                } else {
                    self.app.transcript_scroll = new_scroll;
                }
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
                        // Keep the text in input so user can see/edit it
                        self.app.pending_message = Some(text);
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

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                // If pinned to bottom, unpin and start from max_scroll
                if self.app.transcript_scroll == u16::MAX {
                    self.app.transcript_scroll = self.app.max_scroll.saturating_sub(3);
                } else {
                    self.app.transcript_scroll = self.app.transcript_scroll.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollDown => {
                // If at bottom, re-pin to bottom
                let new_scroll = self.app.transcript_scroll.saturating_add(3);
                if new_scroll >= self.app.max_scroll {
                    self.app.transcript_scroll = u16::MAX;
                } else {
                    self.app.transcript_scroll = new_scroll;
                }
            }
            _ => {}
        }
    }

    #[cfg(test)]
    pub(crate) async fn handle_tui_event(&mut self, event: TuiEvent) -> Result<()> {
        self.handle_tui_event_inner(event).await
    }

    #[cfg(not(test))]
    pub(super) async fn handle_tui_event(&mut self, event: TuiEvent) -> Result<()> {
        self.handle_tui_event_inner(event).await
    }

    async fn handle_tui_event_inner(&mut self, event: TuiEvent) -> Result<()> {
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
                self.append_streaming_assistant_chunk(&chunk);
                self.pin_transcript_to_bottom();
            }
            TuiEvent::Finished {
                output,
                usage,
                tool_results,
            } => {
                self.app.llm_busy = false;
                if !output.is_empty() {
                    if let Some(idx) = self.app.streaming_assistant_idx {
                        match self.app.transcript.get_mut(idx) {
                            Some(TranscriptEntry::Assistant(existing)) if !existing.is_empty() => {
                                if existing != &output {
                                    *existing = output;
                                }
                            }
                            _ => {
                                self.app.transcript.push(TranscriptEntry::Assistant(output));
                                self.app.streaming_assistant_idx =
                                    Some(self.app.transcript.len() - 1);
                            }
                        }
                    } else {
                        self.app.transcript.push(TranscriptEntry::Assistant(output));
                        self.app.streaming_assistant_idx = Some(self.app.transcript.len() - 1);
                    }
                    self.pin_transcript_to_bottom();
                }
                self.app.streaming_assistant_idx = None;
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
                    // Clear input now that the pending message is actually being submitted
                    self.app.input = Self::new_input();
                    self.app
                        .transcript
                        .push(TranscriptEntry::User(pending.clone()));
                    self.pin_transcript_to_bottom();
                    if pending.trim_start().starts_with('.') {
                        self.run_repl_command(&pending).await?;
                        self.refresh_input_chrome();
                    } else {
                        self.start_prompt(pending).await?;
                    }
                }
            }
            TuiEvent::Errored(err) => {
                self.app.llm_busy = false;
                self.app.streaming_assistant_idx = None;
                self.app.transcript.push(TranscriptEntry::Error(err));
                self.pin_transcript_to_bottom();
                self.refresh_input_chrome();

                if let Some(pending) = self.app.pending_message.take() {
                    // Clear input now that the pending message is actually being submitted
                    self.app.input = Self::new_input();
                    self.app
                        .transcript
                        .push(TranscriptEntry::User(pending.clone()));
                    self.pin_transcript_to_bottom();
                    if pending.trim_start().starts_with('.') {
                        self.run_repl_command(&pending).await?;
                        self.refresh_input_chrome();
                    } else {
                        self.start_prompt(pending).await?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) async fn start_prompt(&mut self, text: String) -> Result<()> {
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

    pub(super) fn set_input_text(&mut self, text: &str) {
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

        // Compute replacement bounds so we only replace the token under the cursor.
        let text_before = &line[..pos];
        let word_start = text_before
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        let word_end = line[pos..]
            .find(|c: char| c.is_whitespace())
            .map(|i| pos + i)
            .unwrap_or(line.len());
        self.app.completion_prefix = line[..word_start].to_string();
        self.app.completion_suffix = line[word_end..].to_string();

        self.app.completions = completions;
        self.app.completion_index = 0;
        self.apply_completion();
    }

    pub(super) fn apply_completion(&mut self) {
        if self.app.completions.is_empty() {
            return;
        }
        let (value, _) = &self.app.completions[self.app.completion_index];
        let new_text = format!(
            "{}{}{}",
            self.app.completion_prefix, value, self.app.completion_suffix
        );

        self.set_input_text(&new_text);
    }

    pub(super) fn compute_completions(
        &self,
        line: &str,
        pos: usize,
    ) -> Vec<(String, Option<String>)> {
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
                .map(|c| (format!("{} ", c.name), Some(c.description.to_string())))
                .collect();
            return commands;
        }

        // For multi-part commands, delegate to config's repl_complete
        if cmd.starts_with('.') {
            let args: Vec<&str> = parts[1..].iter().map(|p| p.0).collect();
            let filter = args.last().copied().unwrap_or("");
            return self.config.read().repl_complete(cmd, &args, filter);
        }

        vec![]
    }

    pub(super) async fn run_repl_command(&mut self, line: &str) -> Result<()> {
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
            Ok(outcome) => {
                if matches!(outcome, crate::repl::CommandOutcome::Exit) {
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
}
