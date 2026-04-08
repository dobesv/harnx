use super::*;
use crate::tui::types::{TranscriptEntry, TuiEvent};
use gag::BufferRedirect;
use std::io::Read as _;

fn unique_attachment_display_name(
    attachments: &[crate::tui::types::Attachment],
    original_name: &str,
) -> String {
    if !attachments.iter().any(|a| a.display_name == original_name) {
        return original_name.to_string();
    }

    for idx in 1.. {
        let candidate = format!("{} ({idx})", original_name);
        if !attachments.iter().any(|a| a.display_name == candidate) {
            return candidate;
        }
    }

    unreachable!()
}

fn unique_attachment_storage_path(
    dir: &std::path::Path,
    original_name: &str,
) -> std::path::PathBuf {
    let source_path = std::path::Path::new(original_name);
    let stem = source_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "attachment".to_string());
    let ext = source_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    dir.join(format!("{}-{}{}", stem, uuid::Uuid::new_v4(), ext))
}

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
                    self.app.scroll_state.scroll_up();
                }
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                if self.app.completions.is_empty() {
                    self.history_next();
                } else {
                    self.app.scroll_state.scroll_down();
                }
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                for _ in 0..10 {
                    self.app.scroll_state.scroll_up();
                }
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                for _ in 0..10 {
                    self.app.scroll_state.scroll_down();
                }
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.handle_tab(false).await;
            }
            (KeyCode::BackTab, KeyModifiers::SHIFT) => {
                self.handle_tab(true).await;
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                if !self.app.completions.is_empty() {
                    self.app.completions.clear();
                }
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if self.try_handle_attach_command().await {
                    return Ok(());
                }
                self.app.completions.clear();
                let text = self.app.input.lines().join("\n");
                if !text.trim().is_empty() || !self.app.attachments.is_empty() {
                    // Reset abort signal before each new submission (fix #3)
                    self.abort_signal.reset();
                    // Add to history (fix #4)
                    self.push_history(text.clone());
                    if self.app.llm_busy {
                        // Queue the message to send when LLM finishes
                        // Keep the text in input so user can see/edit it
                        let pending_attachments = self.app.attachments.clone();
                        let pending_attachment_dir = self.app.attachment_dir.clone();
                        self.app.pending_message = Some(crate::tui::types::PendingMessage {
                            text,
                            attachments: pending_attachments,
                            attachment_dir: pending_attachment_dir,
                            paste_count: self.app.paste_count,
                        });
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
                        let msg = crate::tui::types::PendingMessage {
                            text,
                            attachments: std::mem::take(&mut self.app.attachments),
                            attachment_dir: self.app.attachment_dir.take(),
                            paste_count: self.app.paste_count,
                        };
                        self.start_prompt(msg).await?;
                    }
                }
            }
            (KeyCode::Enter, KeyModifiers::SHIFT) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                // Shift+Enter / Ctrl+J inserts a newline - clear pending if any
                if let Some(pending) = self.app.pending_message.take() {
                    self.app.attachments = pending.attachments;
                    self.app.attachment_dir = pending.attachment_dir;
                    self.app.paste_count = pending.paste_count;
                    self.refresh_input_chrome();
                }
                self.app.input.input(TextInput {
                    key: Key::Enter,
                    ..Default::default()
                });
            }
            _ => {
                // Any other key input clears pending message (converts back to draft)
                if let Some(pending) = self.app.pending_message.take() {
                    self.app.attachments = pending.attachments;
                    self.app.attachment_dir = pending.attachment_dir;
                    self.app.paste_count = pending.paste_count;
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

    /// Ensure the attachment temp directory exists, creating it via mkdtemp if needed.
    async fn ensure_attachment_dir(&mut self) -> std::io::Result<std::path::PathBuf> {
        if let Some(ref dir) = self.app.attachment_dir {
            Ok(dir.clone())
        } else {
            let dir = crate::tui::types::create_attachment_dir()?;
            self.app.attachment_dir = Some(dir.clone());
            Ok(dir)
        }
    }

    /// Clean up the attachment temp directory and reset attachment state.
    pub(super) fn cleanup_attachments(&mut self) {
        self.app.attachments.clear();
        if let Some(dir) = self.app.attachment_dir.take() {
            crate::tui::types::cleanup_attachment_dir(&dir);
        }
    }

    /// Check if the last line of input is an `.attach` or `.detach` command.
    /// If so, execute it and return `true`. The command line is removed from
    /// the textarea, preserving any preceding draft text.
    async fn try_handle_attach_command(&mut self) -> bool {
        let last_line = {
            let lines = self.app.input.lines();
            match lines.last() {
                Some(l) => l.trim().to_string(),
                None => return false,
            }
        };

        if last_line.starts_with(".attach ") {
            let path_str = last_line
                .strip_prefix(".attach ")
                .unwrap()
                .trim()
                .to_string();
            let src = std::path::PathBuf::from(&path_str);
            if src.exists() {
                match self.ensure_attachment_dir().await {
                    Ok(dir) => {
                        let original_name = src
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path_str.clone());
                        let display_name =
                            unique_attachment_display_name(&self.app.attachments, &original_name);
                        let dest = unique_attachment_storage_path(&dir, &original_name);
                        if let Err(err) = tokio::fs::copy(&src, &dest).await {
                            self.app.transcript.push(TranscriptEntry::Error(format!(
                                "Failed to copy attachment: {err}"
                            )));
                        } else {
                            self.app.attachments.push(crate::tui::types::Attachment {
                                path: dest,
                                display_name,
                            });
                        }
                    }
                    Err(err) => {
                        self.app.transcript.push(TranscriptEntry::Error(format!(
                            "Failed to create attachment directory: {err}"
                        )));
                    }
                }
            } else {
                self.app.transcript.push(TranscriptEntry::Error(format!(
                    "File not found: {path_str}"
                )));
            }
        } else if last_line == ".detach" {
            self.cleanup_attachments();
        } else if last_line.starts_with(".detach ") {
            let name = last_line
                .strip_prefix(".detach ")
                .unwrap()
                .trim()
                .to_string();
            for attachment in self
                .app
                .attachments
                .iter()
                .filter(|a| a.display_name == name)
            {
                if let Err(err) = std::fs::remove_file(&attachment.path) {
                    self.app.transcript.push(TranscriptEntry::Error(format!(
                        "Failed to remove detached attachment file {}: {err}",
                        attachment.display_name
                    )));
                }
            }
            self.app.attachments.retain(|a| a.display_name != name);
            // If no attachments left, clean up the directory
            if self.app.attachments.is_empty() {
                self.cleanup_attachments();
            }
        } else {
            return false;
        }

        // Remove the last line (the command) and restore remaining text
        let remaining_text = {
            let lines = self.app.input.lines();
            let remaining: Vec<String> = lines[..lines.len() - 1].to_vec();
            remaining.join("\n")
        };
        self.set_input_text(&remaining_text);

        true
    }

    pub(super) async fn handle_paste(&mut self, text: String) {
        if let Some(pending) = self.app.pending_message.take() {
            self.app.attachments = pending.attachments;
            self.app.attachment_dir = pending.attachment_dir;
            self.refresh_input_chrome();
        }
        if !self.app.completions.is_empty() {
            self.app.completions.clear();
        }
        // Normalize line endings: \r\n -> \n, then \r -> \n
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if text.contains('\n') {
            // Multi-line paste: write to temp file and attach
            match self.write_paste_to_attachment_dir(&text).await {
                Ok(attachment) => {
                    self.app.attachments.push(attachment);
                }
                Err(err) => {
                    self.app.transcript.push(TranscriptEntry::Error(format!(
                        "Failed to save pasted text: {err}"
                    )));
                }
            }
        } else {
            // Single-line paste: insert inline
            self.app.input.insert_str(&text);
        }
    }

    async fn write_paste_to_attachment_dir(
        &mut self,
        text: &str,
    ) -> std::io::Result<crate::tui::types::Attachment> {
        let dir = self.ensure_attachment_dir().await?;
        self.app.paste_count += 1;
        let filename = format!("paste-{}.txt", self.app.paste_count);
        let path = dir.join(&filename);
        tokio::fs::write(&path, text).await?;
        Ok(crate::tui::types::Attachment {
            path,
            display_name: filename,
        })
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                for _ in 0..3 {
                    self.app.scroll_state.scroll_up();
                }
            }
            MouseEventKind::ScrollDown => {
                for _ in 0..3 {
                    self.app.scroll_state.scroll_down();
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
            TuiEvent::ToolRoundComplete => {
                // Intermediate tool round — prompt loop continues, don't clear llm_busy.
                // Keep the current assistant streaming entry so follow-up text can
                // continue without inserting a blank separator or synthetic status line.
                self.pin_transcript_to_bottom();
            }
            TuiEvent::Finished { output, usage } => {
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
                if !usage.is_empty() {
                    self.app
                        .transcript
                        .push(TranscriptEntry::System(format!("Usage: {usage}")));
                    self.pin_transcript_to_bottom();
                }
                self.refresh_input_chrome();

                if let Some(pending) = self.app.pending_message.take() {
                    self.submit_pending_message(pending).await?;
                }
            }
            TuiEvent::Errored(err) => {
                self.app.llm_busy = false;
                self.app.streaming_assistant_idx = None;
                self.app.transcript.push(TranscriptEntry::Error(err));
                self.pin_transcript_to_bottom();
                self.refresh_input_chrome();

                if let Some(pending) = self.app.pending_message.take() {
                    self.submit_pending_message(pending).await?;
                }
            }
        }
        Ok(())
    }

    async fn submit_pending_message(
        &mut self,
        pending: crate::tui::types::PendingMessage,
    ) -> Result<()> {
        self.app.input = Self::new_input();
        self.app
            .transcript
            .push(TranscriptEntry::User(pending.text.clone()));
        self.pin_transcript_to_bottom();
        if pending.text.trim_start().starts_with('.') {
            self.app.attachments = pending.attachments;
            self.app.attachment_dir = pending.attachment_dir;
            self.app.paste_count = pending.paste_count;
            self.run_repl_command(&pending.text).await?;
            self.refresh_input_chrome();
        } else {
            self.start_prompt(pending).await?;
        }
        Ok(())
    }

    pub(super) async fn start_prompt(
        &mut self,
        msg: crate::tui::types::PendingMessage,
    ) -> Result<()> {
        self.app.llm_busy = true;

        let config = self.config.clone();
        let abort_signal = self.abort_signal.clone();
        let async_manager = self.async_manager.clone();
        let persistent_manager = self.persistent_manager.clone();
        let pending_async_context = self.pending_async_context.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let result: Result<()> = Self::run_prompt_task(
                msg,
                config,
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

    async fn handle_tab(&mut self, reverse: bool) {
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

        let completions = self.compute_completions(&line, pos).await;
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

    pub(super) async fn compute_completions(
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

            // File path completion for .attach
            if cmd == ".attach" {
                let filter = args.last().copied().unwrap_or("");
                let dir_path;
                let prefix;
                if filter.contains('/') || filter.contains('\\') {
                    let p = std::path::Path::new(filter);
                    dir_path = p
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .to_path_buf();
                    prefix = p
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                } else {
                    dir_path = std::path::PathBuf::from(".");
                    prefix = filter.to_string();
                };
                if let Ok(mut entries) = tokio::fs::read_dir(&dir_path).await {
                    let mut matches = Vec::new();
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.starts_with(&prefix) {
                            let full = if dir_path == std::path::Path::new(".") {
                                name.clone()
                            } else {
                                format!("{}/{}", dir_path.display(), name)
                            };
                            let kind = match entry.file_type().await {
                                Ok(file_type) if file_type.is_dir() => Some("dir".to_string()),
                                _ => None,
                            };
                            matches.push((full, kind));
                        }
                    }
                    return matches;
                }
                return vec![];
            }

            // Attachment name completion for .detach
            if cmd == ".detach" {
                let filter = args.last().copied().unwrap_or("");
                return self
                    .app
                    .attachments
                    .iter()
                    .filter(|a| a.display_name.starts_with(filter))
                    .map(|a| (a.display_name.clone(), None))
                    .collect();
            }

            let filter = args.last().copied().unwrap_or("");
            return self.config.read().repl_complete(cmd, &args, filter);
        }

        vec![]
    }

    pub(super) async fn run_repl_command(&mut self, line: &str) -> Result<()> {
        // Run the command inside a block that owns the lock guards so they are
        // dropped before we touch `self` again for transcript / UI updates.
        let (result, captured) = {
            let config = self.config.clone();
            let abort_signal = self.abort_signal.clone();
            let mut async_manager = self.async_manager.lock().await;
            let mut pending_async_context = self.pending_async_context.lock().await;

            // Capture stdout/stderr so REPL commands that use print!/println!/
            // eprint!/eprintln! don't write raw bytes into the ratatui
            // alternate screen.  After the command finishes we drain the
            // captured text and push it into the TUI transcript.
            let mut stdout_buf = BufferRedirect::stdout().ok();
            let mut stderr_buf = BufferRedirect::stderr().ok();

            let result = crate::repl::run_repl_command(
                &config,
                abort_signal,
                line,
                &mut async_manager,
                &self.persistent_manager,
                &mut pending_async_context,
            )
            .await;

            // Drain captured output before dropping the redirects.
            let captured = drain_captured(&mut stdout_buf, &mut stderr_buf);
            drop(stdout_buf);
            drop(stderr_buf);

            (result, captured)
            // async_manager + pending_async_context guards drop here
        };

        if !captured.is_empty() {
            self.app.transcript.push(TranscriptEntry::System(captured));
            self.pin_transcript_to_bottom();
        }

        match result {
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

/// Drain any captured stdout/stderr into a single trimmed string.
fn drain_captured(
    stdout_buf: &mut Option<BufferRedirect>,
    stderr_buf: &mut Option<BufferRedirect>,
) -> String {
    let mut output = String::new();
    if let Some(ref mut buf) = stdout_buf {
        let _ = buf.read_to_string(&mut output);
    }
    if let Some(ref mut buf) = stderr_buf {
        let mut err_output = String::new();
        let _ = buf.read_to_string(&mut err_output);
        if !err_output.is_empty() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&err_output);
        }
    }
    // Strip ANSI escape codes so TUI transcript stays clean
    let clean = strip_ansi(&output);
    clean.trim().to_string()
}
