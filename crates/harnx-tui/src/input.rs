//! Route keyboard/mouse input and manage the draft buffer, history and focus.

// Keep crate-internal helper paths stable for existing callers.
pub(super) use crate::agent_events::{clean_thought_chunk, concat_text_blocks};
#[cfg(test)]
pub(crate) use crate::input_attachments::render_attachment_preview;

use crate::types::{ExitPhase, ModalState, TranscriptItem, Tui};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use harnx_runtime::nats_session_metadata::SessionMetadataStore;
use ratatui_textarea::{Input as TextInput, Key};
use std::time::Duration;

/// Upper bound on the durable mark-read performed while exiting via idle Ctrl+D.
/// The NATS client retries an unavailable connection for up to its recovery
/// deadline (~15s), and the event loop can't observe the Ctrl+D abort until this
/// handler returns, so an unbounded mark-read would stall the exit. Ctrl+D means
/// "quit now": mark read best-effort, then exit regardless.
const CTRL_D_MARK_READ_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerCommand {
    Agent,
    Session,
}

fn picker_command_for_input(line: &str, pos: usize) -> Option<PickerCommand> {
    let upto_cursor = &line[..pos];
    // Normalise the same way the command parser does: strip leading whitespace,
    // then check that the only remaining content is the bare command name
    // (no argument started after the command).
    let trimmed = upto_cursor.trim_start();
    match trimmed.trim_end() {
        ".agent" => Some(PickerCommand::Agent),
        ".session" => Some(PickerCommand::Session),
        _ => None,
    }
}

impl Tui {
    pub(crate) async fn request_exit(&mut self) {
        if !self.app.llm_busy {
            self.app.should_quit = true;
            return;
        }

        let worker_state = self.exit_worker_state().await;
        self.app.modal = Some(ModalState::ConfirmExit {
            worker_state,
            phase: ExitPhase::Prompting,
        });
    }

    async fn handle_ctrl_d(&mut self) {
        // Preserve Ctrl+D's existing idle abort; busy exit defers it to modal confirmation.
        let idle_exit = !self.app.llm_busy;
        if idle_exit {
            self.abort_signal.set_ctrld();
        }
        // Idle exit is terminal, so a queued or dropped invalidation may leave the cached flag
        // stale. The durable mark-read operation is idempotent; don't gate it on the cache.
        // Bound it so a degraded NATS connection can't stall the exit past the timeout.
        if idle_exit {
            if tokio::time::timeout(
                CTRL_D_MARK_READ_TIMEOUT,
                self.mark_current_session_read(true),
            )
            .await
            .is_err()
            {
                log::warn!(
                    "mark-read timed out after {CTRL_D_MARK_READ_TIMEOUT:?} on Ctrl+D exit; exiting anyway"
                );
            }
        } else {
            self.mark_current_session_read(false).await;
        }
        self.request_exit().await;
    }

    pub(super) async fn handle_ctrl_c(&mut self) {
        // Abort signal goes both to the Tui-level signal (used by
        // dot-commands) and to the in-flight prompt task's own
        // signal (if any). Per-task signals are why we no longer
        // need to "reset" anything before the next submission —
        // the running task can never be un-aborted.
        self.abort_signal.set_ctrlc();
        if let Some(prompt_abort) = &self.current_prompt_abort {
            prompt_abort.set_ctrlc();
        }

        // Fire-and-forget cancel; TUI stays alive so detached spawn is safe.
        // Exit paths that quit must use cancel_sequencing via start_exit_cancel.
        self.cancel_active_remote_session();

        // Discard any queued message — Ctrl+C means "cancel
        // everything", including the message you typed while the
        // task was running.
        self.app.pending_message = None;
        *self.shared_pending_message.lock().await = None;
        // A local signal isn't durable acceptance. The requesting tray stays
        // until the receipt settles the prompt, independently of physical cleanup.
        // Idle Ctrl+C can clear local activity without a follower to retire.
        if self.current_prompt_handle.is_none() {
            self.app.llm_busy = false;
            self.active_remote_session = None;
        }
        // Mark current session as read before clearing state (if unread)
        self.mark_current_session_read(false).await;
    }

    async fn handle_browsing_key(&mut self, key: KeyEvent) -> Result<()> {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => {
                self.app.transcript_browsing = false;
                self.app.transcript_focus = None;
                self.app.transcript_selection_anchor = None;
                self.app.scroll_state.follow = true;
            }
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.handle_up_key(key);
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.handle_down_key(key);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.open_focused_root_item().await;
            }
            (KeyCode::Char('e'), KeyModifiers::NONE) => {
                self.handle_transcript_edit().await?;
            }
            (KeyCode::Char('i'), KeyModifiers::NONE) => {
                self.handle_transcript_insert();
            }
            (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('d'), KeyModifiers::NONE) => {
                self.handle_transcript_delete();
            }
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                self.handle_transcript_copy();
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                self.handle_transcript_rewind();
            }
            (
                KeyCode::Char('g' | '<') | KeyCode::Home,
                KeyModifiers::NONE | KeyModifiers::SHIFT,
            ) => {
                self.app.browsing_view_scroll.scroll_to_top();
            }
            (KeyCode::Char('G' | '>') | KeyCode::End, KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.app.browsing_view_scroll.scroll_to_bottom();
            }
            _ => {} // consume all other keys to prevent bleed to input
        }
        Ok(())
    }

    pub(super) async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(result) = self.handle_exclusive_view_key(key).await {
            return result;
        }

        // Browsing mode guard: when user is navigating history fullscreen
        if self.app.transcript_browsing {
            return self.handle_browsing_key(key).await;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => self.handle_ctrl_d().await,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.handle_ctrl_c().await,
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.handle_up_key(key);
            }
            (KeyCode::Up, KeyModifiers::SHIFT) => {
                self.handle_up_key_shift();
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.handle_down_key(key);
            }
            (KeyCode::Down, KeyModifiers::SHIFT) => {
                self.handle_down_key_shift();
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                if self.app.detail_view_open {
                    for _ in 0..10 {
                        self.app.detail_view_scroll.scroll_up();
                    }
                    return Ok(());
                }
                for _ in 0..10 {
                    self.app.scroll_state.scroll_up();
                }
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                if self.app.detail_view_open {
                    for _ in 0..10 {
                        self.app.detail_view_scroll.scroll_down();
                    }
                    return Ok(());
                }
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
                if self.app.detail_view_open {
                    self.app.detail_view_open = false;
                    // Return to browsing view (transcript_browsing stays true if it was true)
                } else if self.app.transcript_focus.is_some() {
                    self.app.transcript_focus = None;
                    self.app.transcript_selection_anchor = None;
                    self.app.transcript_browsing = false;
                    self.app.scroll_state.follow = true;
                    // Mark read on exit from transcript focus (if unread)
                    self.mark_current_session_read(false).await;
                } else if !self.app.completions.is_empty() {
                    self.app.completions.clear();
                } else {
                    // ESC with no special state: mark read (if unread)
                    self.mark_current_session_read(false).await;
                }
            }
            // D4: Keyboard actions on selected transcript item(s)
            // All mutation shortcuts are blocked while the detail view is open.
            (KeyCode::Char('e'), KeyModifiers::NONE) if self.app.transcript_focus.is_some() => {
                self.handle_transcript_edit().await?;
            }
            (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('d'), KeyModifiers::NONE)
                if self.app.transcript_focus.is_some() =>
            {
                self.handle_transcript_delete();
            }
            (KeyCode::Char('i'), KeyModifiers::NONE) if self.app.transcript_focus.is_some() => {
                self.handle_transcript_insert();
            }
            (KeyCode::Char('c'), KeyModifiers::NONE) if self.app.transcript_focus.is_some() => {
                self.handle_transcript_copy();
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) if self.app.transcript_focus.is_some() => {
                self.handle_transcript_rewind();
            }
            (KeyCode::Enter, KeyModifiers::NONE) if self.app.transcript_focus.is_some() => {
                self.open_focused_root_item().await;
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.handle_enter_key().await?;
            }
            (KeyCode::Enter, KeyModifiers::SHIFT) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                // Shift+Enter / Ctrl+J inserts a newline - clear pending if any
                if let Some(pending) = self.app.pending_message.take() {
                    self.app.attachments = pending.attachments;
                    self.app.attachment_dir = pending.attachment_dir;
                    self.app.paste_count = pending.paste_count;
                    self.clear_shared_pending_message().await;
                    self.refresh_input_chrome();
                }
                self.app.input.input(TextInput {
                    key: Key::Enter,
                    ..Default::default()
                });
            }
            (
                KeyCode::Char('g' | '<') | KeyCode::Home,
                KeyModifiers::NONE | KeyModifiers::SHIFT,
            ) if self.app.transcript_focus.is_some() => {
                self.app.scroll_state.scroll_to_top();
            }
            (KeyCode::Char('G' | '>') | KeyCode::End, KeyModifiers::NONE | KeyModifiers::SHIFT)
                if self.app.transcript_focus.is_some() =>
            {
                self.app.scroll_state.scroll_to_bottom();
            }
            _ => {
                // While a transcript item is focused all unhandled keys are
                // silently consumed — they must not leak into the input widget.
                // (The specific action keys e/d/i/c/r are handled above; anything
                // else is irrelevant when focus is on a history item.)
                if self.app.transcript_focus.is_some() {
                    return Ok(());
                }
                // First character typed marks the current session read when needed.
                self.mark_current_session_read(false).await;
                // Exit history preview on any editing key — keep current content as new draft
                if self.app.history_preview {
                    self.app.history_index = None;
                    self.app.history_preview = false;
                    self.refresh_input_chrome();
                }
                // Any other key input clears pending message (converts back to draft)
                if let Some(pending) = self.app.pending_message.take() {
                    self.app.attachments = pending.attachments;
                    self.app.attachment_dir = pending.attachment_dir;
                    self.app.paste_count = pending.paste_count;
                    self.clear_shared_pending_message().await;
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

    async fn handle_enter_key(&mut self) -> Result<()> {
        if self.try_handle_attach_command().await || self.has_root_cancellation() {
            return Ok(());
        }
        self.app.completions.clear();
        let text = self.app.input.lines().join("\n");
        if text.trim().is_empty() && self.app.attachments.is_empty() {
            return Ok(());
        }
        self.mark_current_session_read(false).await;
        self.abort_signal.reset();
        self.push_history(text.clone());
        if self.app.llm_busy {
            self.queue_busy_input(text).await;
            return Ok(());
        }
        if text.trim_start().starts_with('.') {
            return self.submit_dot_command(text).await;
        }
        if let Some(modal) = self.check_picker_modal().await {
            self.app.modal = Some(modal);
            return Ok(());
        }
        let attachments_snapshot = self.app.attachments.clone();
        self.app.transcript.push(TranscriptItem::UserText {
            text: text.clone(),
            seq: None,
            timestamp: Some(chrono::Utc::now()),
        });
        crate::lifecycle::append_markdown_links(&mut self.app.transcript, &text);
        self.render_submitted_attachments(&attachments_snapshot)
            .await;
        self.pin_transcript_to_bottom();
        self.app.input = Self::new_input();
        let msg = crate::types::PendingMessage {
            text: text.clone(),
            attachments: std::mem::take(&mut self.app.attachments),
            attachment_dir: self.app.attachment_dir.take(),
            paste_count: self.app.paste_count,
        };
        self.start_prompt(msg).await
    }

    async fn handle_exclusive_view_key(&mut self, key: KeyEvent) -> Option<Result<()>> {
        if self.handle_cancellation_or_child_key(key) {
            return Some(Ok(()));
        }
        if self.app.modal.is_some() {
            return Some(self.handle_modal_key(key).await);
        }
        if self.app.detail_view_open {
            Some(self.handle_detail_view_key(key).await)
        } else if !self.app.subagent_view_stack.is_empty() {
            self.handle_subagent_view_key(key).await;
            Some(Ok(()))
        } else {
            None
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        if let Some(ModalState::ConfirmToolUse(state)) = self.app.modal.as_mut() {
            for _ in 0..3 {
                if up {
                    state.scroll.scroll_up();
                } else {
                    state.scroll.scroll_down();
                }
            }
            return;
        }
        let state = if self.app.detail_view_open {
            &mut self.app.detail_view_scroll
        } else if self.scroll_open_subagent(up) {
            return;
        } else if self.app.transcript_browsing {
            &mut self.app.browsing_view_scroll
        } else {
            &mut self.app.scroll_state
        };
        for _ in 0..3 {
            if up {
                state.scroll_up();
            } else {
                state.scroll_down();
            }
        }
    }

    /// Clear the shared pending message so the prompt task does not consume a
    /// stale value after the user cancels or edits the pending draft.
    pub(super) async fn clear_shared_pending_message(&self) {
        *self.shared_pending_message.lock().await = None;
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
        self.app.history_preview = false;
    }

    fn input_is_blank(&self) -> bool {
        self.app.input.lines().join("\n").is_empty()
    }

    fn find_prev_navigable(&self, start: usize) -> Option<usize> {
        let mut focus = start;
        while focus > 0 {
            focus -= 1;
            if self.app.transcript[focus].is_navigable() {
                return Some(focus);
            }
        }
        None
    }

    fn find_next_navigable(&self, mut focus: usize) -> Option<usize> {
        while focus + 1 < self.app.transcript.len() {
            focus += 1;
            if self.app.transcript[focus].is_navigable() {
                return Some(focus);
            }
        }
        None
    }

    fn handle_up_key(&mut self, key: KeyEvent) {
        if self.app.detail_view_open {
            self.app.detail_view_scroll.scroll_up();
            return;
        }

        if !self.app.completions.is_empty() {
            self.app.scroll_state.scroll_up();
        } else if let Some(focus) = self.app.transcript_focus {
            if let Some(prev) = self.find_prev_navigable(focus) {
                self.app.transcript_focus = Some(prev);
                self.app.transcript_browsing = true;
                self.app.scroll_state.follow = false;
                self.app.scroll_to_focused_item = true;
                self.app.transcript_selection_anchor = None;
            } else {
                self.app.transcript_focus = None;
                self.app.transcript_selection_anchor = None;
                self.app.transcript_browsing = false;
                // Do NOT restore follow here — user is entering history preview
                // and the transcript position should stay where it is.
                // follow is restored by Esc or when a new message is submitted.

                let before = self.app.history_index;
                self.history_prev();
                let moved = self.app.history_index.is_some()
                    && (self.app.history_index != before || self.app.history_preview);
                if moved {
                    self.app.history_preview = true;
                    self.refresh_input_chrome();
                }
            }
        } else if self.input_is_blank() && !self.app.transcript.is_empty() {
            if let Some(prev) = self.find_prev_navigable(self.app.transcript.len()) {
                self.app.transcript_focus = Some(prev);
                self.app.transcript_browsing = true;
                self.app.scroll_state.follow = false;
                self.app.scroll_to_focused_item = true;
                self.app.transcript_selection_anchor = None;
            } else {
                let before = self.app.history_index;
                self.history_prev();
                let moved = self.app.history_index.is_some()
                    && (self.app.history_index != before || self.app.history_preview);
                if moved {
                    self.app.history_preview = true;
                    self.refresh_input_chrome();
                }
            }
        } else if self.app.history_preview || self.input_is_blank() {
            let before = self.app.history_index;
            self.history_prev();
            let moved = self.app.history_index.is_some()
                && (self.app.history_index != before || self.app.history_preview);
            if moved {
                self.app.history_preview = true;
                self.refresh_input_chrome();
            }
        } else {
            self.app.input.input(TextInput::from(key));
        }
    }

    fn handle_down_key(&mut self, key: KeyEvent) {
        if self.app.detail_view_open {
            self.app.detail_view_scroll.scroll_down();
            return;
        }

        if !self.app.completions.is_empty() {
            self.app.scroll_state.scroll_down();
        } else if let Some(focus) = self.app.transcript_focus {
            if let Some(next) = self.find_next_navigable(focus) {
                self.app.transcript_focus = Some(next);
                self.app.transcript_browsing = true;
                self.app.scroll_state.follow = false;
                self.app.scroll_to_focused_item = true;
                self.app.transcript_selection_anchor = None;
            } else {
                self.app.transcript_focus = None;
                self.app.transcript_selection_anchor = None;
                self.app.transcript_browsing = false;
                self.app.scroll_state.follow = true;
            }
        } else if self.app.history_preview {
            self.history_next();
            if self.app.history_index.is_none() {
                self.app.history_preview = false;
            }
            self.refresh_input_chrome();
        } else {
            self.app.input.input(TextInput::from(key));
        }
    }

    fn handle_up_key_shift(&mut self) {
        // If no focus yet, initialize it at the last navigable item (same as plain Up)
        let focus = if let Some(f) = self.app.transcript_focus {
            f
        } else if self.input_is_blank() && !self.app.transcript.is_empty() {
            if let Some(last) = self.find_prev_navigable(self.app.transcript.len()) {
                self.app.transcript_focus = Some(last);
                self.app.scroll_state.follow = false;
                self.app.scroll_to_focused_item = true;
                last
            } else {
                return;
            }
        } else {
            return;
        };
        if self.app.transcript_selection_anchor.is_none() {
            self.app.transcript_selection_anchor = Some(focus);
        }
        if let Some(prev) = self.find_prev_navigable(focus) {
            self.app.transcript_focus = Some(prev);
            self.app.scroll_state.follow = false;
            self.app.scroll_to_focused_item = true;
        }
    }

    fn handle_down_key_shift(&mut self) {
        let Some(focus) = self.app.transcript_focus else {
            return; // Shift+Down has no effect without an active focus
        };
        if self.app.transcript_selection_anchor.is_none() {
            self.app.transcript_selection_anchor = Some(focus);
        }
        if let Some(next) = self.find_next_navigable(focus) {
            self.app.transcript_focus = Some(next);
            self.app.scroll_state.follow = false;
            self.app.scroll_to_focused_item = true;
        }
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

        let picker_command = picker_command_for_input(&line, pos);
        let completions = self.compute_completions(&line, pos).await;
        if completions.is_empty() {
            match picker_command {
                Some(PickerCommand::Agent) => {
                    self.open_agent_picker().await;
                    return;
                }
                Some(PickerCommand::Session) => {
                    self.open_session_picker().await;
                    return;
                }
                None => return,
            }
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
        let parts = crate::completion::command_parts(&line[..pos]);
        let Some(cmd) = parts.first().copied() else {
            return vec![];
        };
        if !cmd.starts_with('.') {
            return vec![];
        }
        if parts.len() == 1 {
            return crate::completion::command_name_completions(cmd);
        }
        self.command_argument_completions(cmd, parts[1..].to_vec())
            .await
    }
}

impl Tui {
    /// Mark the current session as read, using the cached unread flag as a fast path unless
    /// `force` is set for a terminal action.
    ///
    /// The cache can be stale because: (1) `run_loop_inner` handles key input before draining
    /// `event_rx`, so a queued `SessionReadInvalidation` hasn't updated the flag yet, and
    /// (2) the session activity monitor stops while a prompt is in-flight, so non-durable
    /// read-invalidations published at TurnEnd can be dropped. Callers on terminal exit
    /// paths (idle Ctrl+D) should pass `force=true` to bypass the cache and hit durable KV
    /// directly; `mark_read` is idempotent. Non-exit presence actions (Esc, first keystroke,
    /// Ctrl+C) use `force=false` since they fire repeatedly and self-correct.
    ///
    /// Boxed to keep `handle_key`'s future frame compact and avoid stack overflow in tests
    /// (the async body contains await chains that inflate the stack size).
    fn mark_current_session_read(
        &mut self,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if !force && !self.app.current_session_unread {
                return;
            }
            // Use session_activity_destination to resolve the current session even when idle
            // (when user presence typing, ESC, CTRL-C, etc. occur, active_remote_session may be None).
            let Some((session_id, cluster)) = self.session_activity_destination() else {
                return;
            };
            let config = self.config.read().clone();
            let jetstream = match config.nats_jetstream(&cluster).await {
                Ok(js) => js,
                Err(e) => {
                    log::warn!("Failed to get jetstream for mark-read: {e:#}");
                    return;
                }
            };
            let store = match SessionMetadataStore::ensure(&jetstream, 1).await {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("Failed to ensure metadata store for mark-read: {e:#}");
                    return;
                }
            };
            if let Err(e) = store.mark_read(&session_id).await {
                log::warn!("Failed to mark session as read: {e:#}");
                return;
            }
            self.app.current_session_unread = false;
            self.refresh_input_chrome();
        })
    }
}

impl Tui {
    /// Compute selected transcript index range [min, max] from focus+anchor.
    fn get_selected_index_range(&self) -> (usize, usize) {
        let focus = self
            .app
            .transcript_focus
            .expect("transcript_focus required");
        let anchor = self.app.transcript_selection_anchor.unwrap_or(focus);
        (focus.min(anchor), focus.max(anchor))
    }

    /// Get seq range (from_seq, to_seq) for selected items.
    /// Returns None when selected items do not have sequence numbers.
    fn selected_seq_range(&self) -> Option<(usize, usize)> {
        let (start_idx, end_idx) = self.get_selected_index_range();
        let from = self
            .app
            .transcript
            .get(start_idx)
            .and_then(|item| item.seq());
        let to = self.app.transcript.get(end_idx).and_then(|item| item.seq());
        match (from, to) {
            (Some(from), Some(to)) => Some((from.min(to), from.max(to))),
            _ => None,
        }
    }

    /// Get text content from transcript item for copy/insert operations.
    pub(crate) fn get_transcript_item_text(item: &TranscriptItem) -> Option<String> {
        match item {
            TranscriptItem::UserText { text, .. } => Some(text.clone()),
            TranscriptItem::AssistantText { text, .. } => Some(text.clone()),
            TranscriptItem::CompactionMarker { detail_text, .. } => Some(detail_text.clone()),
            TranscriptItem::ToolCall {
                tool_name,
                body: Some(crate::types::ToolCallBody::Yaml(body)),
                ..
            }
            | TranscriptItem::ToolCall {
                tool_name,
                body: Some(crate::types::ToolCallBody::Markdown(body)),
                ..
            } => Some(format!("{}({})", tool_name, body)),
            TranscriptItem::ToolCall { tool_name, .. } => Some(format!("{}()", tool_name)),
            TranscriptItem::ToolResultMarkdown { text, .. } => Some(text.clone()),
            TranscriptItem::MarkdownLink { text, url } => {
                if text.is_empty() || text == url {
                    Some(url.clone())
                } else {
                    Some(format!("{text}: {url}"))
                }
            }
            _ => None,
        }
    }

    /// Handle 'e' key: open edit command for selected item(s).
    pub(super) async fn handle_transcript_edit(&mut self) -> Result<()> {
        let Some((from, to)) = self.selected_seq_range() else {
            return Ok(());
        };
        let cmd = if from == to {
            format!(".edit message {}", from)
        } else {
            format!(".edit message {}-{}", from, to)
        };
        self.run_command(&cmd).await?;
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = false;
        self.app.scroll_state.follow = true;
        Ok(())
    }

    /// Handle 'd' or Delete key: open delete confirmation modal.
    pub(super) fn handle_transcript_delete(&mut self) {
        let Some((from, to)) = self.selected_seq_range() else {
            return;
        };
        self.app.modal = Some(crate::types::ModalState::ConfirmDelete { from, to });
    }

    /// Handle 'i' key: copy item text into input field, clear focus.
    fn handle_transcript_insert(&mut self) {
        let focus = match self.app.transcript_focus {
            Some(f) => f,
            None => return,
        };
        let item = match self.app.transcript.get(focus) {
            Some(item) => item.clone(),
            None => return,
        };
        if let Some(text) = Self::get_transcript_item_text(&item) {
            self.set_input_text(&text);
        }
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = false;
        self.app.scroll_state.follow = true;
    }

    /// Handle 'c' key: copy item text to clipboard.
    pub(super) fn handle_transcript_copy(&mut self) {
        if let Some(text) = self
            .app
            .transcript_focus
            .and_then(|focus| self.app.transcript.get(focus))
            .and_then(Self::get_transcript_item_text)
        {
            let _ = harnx_runtime::utils::set_text(&text);
        }
    }

    /// Handle 'r' key: open rewind confirmation modal.
    ///
    /// Always rewinds to the *earliest* selected item regardless of selection
    /// direction, so Shift+selecting up vs down yields the same target.
    pub(super) fn handle_transcript_rewind(&mut self) {
        let focus = self
            .app
            .transcript_focus
            .expect("transcript_focus required");
        let focus = match self.app.transcript_selection_anchor {
            Some(anchor) => focus.min(anchor),
            None => focus,
        };
        let item = match self.app.transcript.get(focus) {
            Some(item) => item,
            None => return,
        };
        let Some(seq) = item.seq() else {
            return;
        };
        let user_text = match item {
            TranscriptItem::UserText { text, .. } => Some(text.clone()),
            _ => None,
        };
        self.app.modal = Some(crate::types::ModalState::ConfirmRewind { seq, user_text });
    }
}
