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
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Upper bound on the durable mark-read performed while exiting via idle Ctrl+D.
/// The NATS client retries an unavailable connection for up to its recovery
/// deadline (~15s), and the event loop can't observe the Ctrl+D abort until this
/// handler returns, so an unbounded mark-read would stall the exit. Ctrl+D means
/// "quit now": mark read best-effort, then exit regardless.
const CTRL_D_MARK_READ_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSE_PAGE_LINES: usize = 10;

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

    pub(super) async fn handle_ctrl_d(&mut self) {
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
            self.mark_current_session_read_background();
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

        // Cancel the background history/picker/info work this frontend owns
        // (design 9): an in-flight rebuild, history command, info-overlay
        // load, picker refresh, or "New session" reservation waiting on a
        // recovering broker stops here. Each job has its own signal, so
        // later work is never poisoned and nothing is left un-aborted.
        self.cancel_background_history_work();

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
        self.mark_current_session_read_background();
    }

    /// Enter fullscreen transcript browsing at the last navigable item without
    /// mutating composer, note, or form state. Returns `false` when the
    /// transcript has no navigable items, leaving focus unchanged.
    pub(super) fn enter_transcript_browsing(&mut self) -> bool {
        let Some(target) = self.find_prev_navigable(self.app.transcript.len()) else {
            return false;
        };
        self.clear_footer_notice();
        self.app.completions.clear();
        self.app.transcript_focus = Some(target);
        self.app.transcript_browsing = true;
        self.app.scroll_state.follow = false;
        self.app.scroll_to_focused_item = true;
        self.app.transcript_selection_anchor = None;
        self.refresh_input_chrome();
        true
    }

    fn exit_transcript_browsing(&mut self) {
        self.clear_footer_notice();
        self.app.transcript_browsing = false;
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.scroll_state.follow = true;
        self.refresh_input_chrome();
        // Mark read on exit from transcript focus/browsing (if unread).
        self.mark_current_session_read_background();
    }

    fn scroll_browsing_page(&mut self, up: bool) {
        self.clear_footer_notice();
        for _ in 0..BROWSE_PAGE_LINES {
            if up {
                self.app.browsing_view_scroll.scroll_up();
            } else {
                self.app.browsing_view_scroll.scroll_down();
            }
        }
    }

    fn handle_ordinary_esc(&mut self) {
        self.clear_footer_notice();
        if self.app.detail_view_open {
            self.app.detail_view_open = false;
            // Return to browsing view (transcript_browsing stays true if it was true)
        } else if self.app.transcript_focus.is_some() {
            self.exit_transcript_browsing();
        } else if !self.app.completions.is_empty() {
            self.app.completions.clear();
        } else {
            // ESC with no special state: mark read (if unread) and enter browse.
            self.mark_current_session_read_background();
            if !self.enter_transcript_browsing() {
                self.app.show_footer_notice("Nothing to browse");
            }
        }
    }

    pub(super) async fn handle_browsing_key(&mut self, key: KeyEvent) -> Result<()> {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => {
                self.handle_browsing_esc();
            }
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.handle_up_key(key);
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.handle_down_key(key);
            }
            (KeyCode::Up, KeyModifiers::SHIFT) => {
                self.handle_up_key_shift();
            }
            (KeyCode::Down, KeyModifiers::SHIFT) => {
                self.handle_down_key_shift();
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                self.scroll_browsing_page(true);
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                self.scroll_browsing_page(false);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.clear_footer_notice();
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
                self.clear_footer_notice();
                self.app.browsing_view_scroll.scroll_to_top();
            }
            (KeyCode::Char('G' | '>') | KeyCode::End, KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.clear_footer_notice();
                self.app.browsing_view_scroll.scroll_to_bottom();
            }
            _ => {} // consume all other keys to prevent bleed to input
        }
        Ok(())
    }

    /// Top-level browse Esc exits browsing; while a confirmation is pending it
    /// returns to the guarded tray, not the hidden composer (plan decision 5).
    fn handle_browsing_esc(&mut self) {
        self.exit_transcript_browsing();
        if self.interaction_pending() {
            self.arm_interaction_return_guard();
        }
    }

    /// Type-erased seam for the run loop (PR #2835 Windows CI query-depth
    /// overflow): `Pin<Box<dyn Future>>` stops rustc's cumulative layout
    /// walk of the loop's future at this await; see AGENTS.md "Future size
    /// and type erasure in harnx-runtime".
    pub(super) fn handle_key_erased(
        &mut self,
        key: KeyEvent,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + '_>> {
        Box::pin(self.handle_key(key))
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
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => self.pick_attachment_files().await,
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
            (KeyCode::PageUp, KeyModifiers::NONE) => self.scroll_input_page(true),
            (KeyCode::PageDown, KeyModifiers::NONE) => self.scroll_input_page(false),
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.handle_tab(false).await;
            }
            (KeyCode::BackTab, KeyModifiers::SHIFT) => {
                self.handle_tab(true).await;
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                self.handle_ordinary_esc();
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
                self.clear_footer_notice();
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
            _ => self.handle_draft_key(key).await,
        }
        Ok(())
    }

    async fn handle_draft_key(&mut self, key: KeyEvent) {
        // Unhandled keys on a focused transcript item must not leak into the draft.
        if self.app.transcript_focus.is_some() {
            return;
        }
        self.clear_footer_notice();
        // First character typed marks the current session read when needed.
        self.mark_current_session_read_background();
        // Exit history preview on any editing key — keep current content as new draft.
        if self.app.history_preview {
            self.app.history_index = None;
            self.app.history_preview = false;
            self.refresh_input_chrome();
        }
        // Any other key input clears pending message (converts back to draft).
        if let Some(pending) = self.app.pending_message.take() {
            self.app.attachments = pending.attachments;
            self.app.attachment_dir = pending.attachment_dir;
            self.app.paste_count = pending.paste_count;
            self.clear_shared_pending_message().await;
            self.refresh_input_chrome();
        }
        // Clear completions on any non-tab key.
        if !self.app.completions.is_empty() {
            self.app.completions.clear();
        }
        self.app.input.input(TextInput::from(key));
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
        self.mark_current_session_read_background();
        self.abort_signal.reset();
        self.push_history(text.clone());
        if self.app.llm_busy {
            self.queue_busy_input(text).await;
            return Ok(());
        }
        if text.trim_start().starts_with('.') {
            return self.submit_dot_command(text).await;
        }
        if self.app.modal.is_none() {
            // Opening an initial picker when agent/session is missing must
            // not wait on the broker: the session list loads in the
            // background (design 9).
            self.check_picker_modal().await;
        }
        if self.app.modal.is_some() {
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
        self.reset_pending_attachment_view();
        let msg = crate::types::PendingMessage {
            text: text.clone(),
            attachments: std::mem::take(&mut self.app.attachments),
            attachment_dir: self.app.attachment_dir.take(),
            paste_count: self.app.paste_count,
        };
        self.start_prompt(msg).await
    }

    async fn handle_exclusive_view_key(&mut self, key: KeyEvent) -> Option<Result<()>> {
        if self.handle_child_answer_key(key) {
            return Some(Ok(()));
        }
        if let Some(res) = self.pending_reading_key(key).await {
            return Some(res);
        }
        if self.app.modal.is_none() && self.app.archive_browser.is_some() {
            if self.app.detail_view_open {
                return Some(self.handle_detail_view_key(key).await);
            }
            return Some(self.handle_archive_key(key));
        }
        if self.handle_cancellation_or_child_key(key) {
            return Some(Ok(()));
        }
        if self.app.modal.is_some() {
            return Some(self.handle_modal_key(key).await);
        }
        self.handle_editor_view_key(key).await
    }

    // qual:allow(iosp) — Keyboard routing selects the exclusive view and delegates its handler.
    async fn handle_editor_view_key(&mut self, key: KeyEvent) -> Option<Result<()>> {
        if self.handle_pending_attachment_key(key).await {
            return Some(Ok(()));
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

    /// A pending confirmation stays readable: keys route to the active read
    /// view instead of the modal (plan decision 3/6). None when no read view
    /// holds focus or a cancellation modal is up.
    async fn pending_reading_key(&mut self, key: KeyEvent) -> Option<Result<()>> {
        if self.interaction_reading() && self.cancellation.is_none() {
            return Some(self.handle_pending_reading_key(key).await);
        }
        None
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        if let Some(ModalState::ConfirmToolUse(state)) = self.app.modal.as_mut() {
            scroll_mouse_state(&mut state.scroll, up);
            return;
        }
        let state = if self.app.detail_view_open {
            &mut self.app.detail_view_scroll
        } else if let Some(view) = self
            .app
            .archive_browser
            .as_mut()
            .and_then(|browser| browser.views.last_mut())
        {
            &mut view.scroll
        } else if self.scroll_open_subagent(up) {
            return;
        } else if self.app.transcript_browsing {
            &mut self.app.browsing_view_scroll
        } else {
            &mut self.app.scroll_state
        };
        scroll_mouse_state(state, up);
    }

    /// Preserve detail-view routing even when a page key reaches the composer
    /// fallback; exclusive overlays normally handle these keys first.
    pub(super) fn scroll_input_page(&mut self, up: bool) {
        self.clear_footer_notice();
        let scroll = if self.app.detail_view_open {
            &mut self.app.detail_view_scroll
        } else {
            &mut self.app.scroll_state
        };
        for _ in 0..BROWSE_PAGE_LINES {
            if up {
                scroll.scroll_up();
            } else {
                scroll.scroll_down();
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

    fn preview_history_prev(&mut self) {
        self.history_prev();
        self.refresh_input_chrome();
    }

    fn handle_up_key(&mut self, key: KeyEvent) {
        self.clear_footer_notice();
        if self.app.detail_view_open {
            self.app.detail_view_scroll.scroll_up();
            return;
        }

        if self.focus_pending_from_input() {
            return;
        }

        if !self.app.completions.is_empty() {
            self.app.scroll_state.scroll_up();
        } else if let Some(focus) = self.app.transcript_focus {
            self.up_from_focused_transcript_row(focus);
        } else if self.input_is_blank() && self.enter_transcript_browsing() {
            // Entered transcript browsing at the last navigable item.
        } else if self.interaction_reading() {
            // A pending confirmation owns the keyboard while reading; arrows
            // must never fall through to the hidden composer or history
            // (review B2).
            self.show_pending_interaction_notice();
        } else if self.app.history_preview || self.input_is_blank() {
            self.preview_history_prev();
        } else {
            self.app.input.input(TextInput::from(key));
        }
    }

    fn up_from_focused_transcript_row(&mut self, focus: usize) {
        if let Some(prev) = self.find_prev_navigable(focus) {
            self.app.transcript_focus = Some(prev);
            self.app.transcript_browsing = true;
            self.app.scroll_state.follow = false;
            self.app.scroll_to_focused_item = true;
            self.app.transcript_selection_anchor = None;
        } else if self.interaction_pending() {
            // Clamped at the first item: no composer-history fallback
            // while a confirmation is pending (plan decision 5).
        } else {
            self.app.transcript_focus = None;
            self.app.transcript_selection_anchor = None;
            self.app.transcript_browsing = false;
            // Do NOT restore follow here — user is entering history preview
            // and the transcript position should stay where it is.
            // follow is restored by Esc or when a new message is submitted.
            self.preview_history_prev();
        }
    }

    fn handle_down_key(&mut self, key: KeyEvent) {
        self.clear_footer_notice();
        if self.app.detail_view_open {
            self.app.detail_view_scroll.scroll_down();
            return;
        }

        if !self.app.completions.is_empty() {
            self.app.scroll_state.scroll_down();
            return;
        }
        if let Some(focus) = self.app.transcript_focus {
            self.down_from_focused_transcript_row(focus);
        } else if self.interaction_reading() {
            // A pending confirmation owns the keyboard while reading; Down
            // must never fall through to the hidden composer or history
            // (review B2).
            self.show_pending_interaction_notice();
        } else if self.app.history_preview {
            self.down_history_preview();
        } else {
            self.app.input.input(TextInput::from(key));
        }
    }

    fn down_from_focused_transcript_row(&mut self, focus: usize) {
        if let Some(next) = self.find_next_navigable(focus) {
            self.app.transcript_focus = Some(next);
            self.app.transcript_browsing = true;
            self.app.scroll_state.follow = false;
            self.app.scroll_to_focused_item = true;
            self.app.transcript_selection_anchor = None;
        } else if !self.app.attachments.is_empty() && !self.interaction_pending() {
            self.focus_pending_attachment(0);
        } else {
            self.exit_browse_to_tray();
        }
    }

    fn down_history_preview(&mut self) {
        self.history_next();
        self.refresh_input_chrome();
    }

    /// Down past the last item leaves browsing; while a confirmation is
    /// pending, that returns to the guarded tray, not the composer (plan
    /// decision 5).
    fn exit_browse_to_tray(&mut self) {
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = false;
        self.app.scroll_state.follow = true;
        if self.interaction_pending() {
            self.arm_interaction_return_guard();
        }
    }

    fn handle_up_key_shift(&mut self) {
        self.clear_footer_notice();
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
        self.clear_footer_notice();
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
        self.app.history_preview = true;
        let text = self.app.history[next_index].clone();
        self.set_input_text(&text);
    }

    fn history_next(&mut self) {
        match self.app.history_index {
            None => {} // Not in history navigation
            Some(0) => {
                // Back to draft, which also ends the preview.
                self.app.history_index = None;
                let draft = self.app.history_draft.clone();
                self.set_input_text(&draft);
                self.app.history_preview = false;
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
                    self.open_session_picker();
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

    /// Fire-and-forget variant of `mark_current_session_read(false)`: the
    /// durable mark-read writes can wait on managed-local acquisition while
    /// the broker recovers, so presence keystrokes (draft typing, Esc,
    /// Enter, Ctrl-C) must not block the input/event loop on them (design
    /// 9). The unread cache is cleared optimistically — mark_read is
    /// idempotent and the activity monitor self-corrects a failed write —
    /// and the write runs detached: its failure paths only log, like the
    /// inline variant's.
    fn mark_current_session_read_background(&mut self) {
        if !self.app.current_session_unread {
            return;
        }
        // Use session_activity_destination to resolve the current session even
        // when idle (presence typing, ESC, CTRL-C); active_remote_session may
        // be None.
        let Some((storage_key, cluster)) = self.session_activity_destination() else {
            return;
        };
        self.app.current_session_unread = false;
        self.refresh_input_chrome();
        let config = self.config.read().clone();
        // Detached spawn is safe: the TUI outlives this fire-and-forget write.
        tokio::spawn(async move {
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
            if let Err(e) = store.mark_read(&storage_key).await {
                log::warn!("Failed to mark session as read: {e:#}");
            }
        });
    }
}

/// What pressing `e` on a browse selection does: dispatch an `.edit message`
/// command or refuse with a footer notice.
enum EditOutcome {
    Notice(String),
    Edit { from: usize, to: usize },
}

impl Tui {
    /// Compute selected transcript index range [min, max] from focus+anchor.
    ///
    /// Returns `None` when nothing is focused (browse mode with no focus),
    /// so callers must degrade gracefully instead of unwrapping.
    fn get_selected_index_range(&self) -> Option<(usize, usize)> {
        let focus = self.app.transcript_focus?;
        let anchor = self.app.transcript_selection_anchor.unwrap_or(focus);
        Some((focus.min(anchor), focus.max(anchor)))
    }

    /// Suffix a browse-action refusal with "item" or "selection" depending on
    /// whether more than one item is highlighted. With no focus, "item".
    fn browse_refusal_noun(&self, prefix: &str) -> String {
        match self.get_selected_index_range() {
            Some((start_idx, end_idx)) if start_idx != end_idx => {
                format!("{prefix} selection")
            }
            _ => format!("{prefix} item"),
        }
    }

    /// Notice for browse mutations refused because a turn is running. A
    /// pending HITL approval also holds a turn open, so the wording covers
    /// both.
    pub(super) fn show_busy_notice(&mut self) {
        self.app.show_footer_notice(
            "Busy: wait for the current turn to finish (or answer pending approval)",
        );
    }

    /// Notice for browse/confirmation actions refused because the displayed
    /// rows belong to an older target or superseded view revision.
    pub(super) fn show_stale_transcript_notice(&mut self) {
        self.app.show_footer_notice(
            "Session changed — wait for current transcript to load before editing",
        );
    }

    /// Clear any transient footer notice. Called on focus navigation and view
    /// changes so the hint legend (e.g. `ESC/close`) stays discoverable.
    fn clear_footer_notice(&mut self) {
        self.app.footer_notice = None;
    }

    /// Get seq range (from_seq, to_seq) for selected items.
    ///
    /// Scans the whole selection instead of sampling only the boundary items:
    /// a range can start or end on an item without a seq (e.g. a markdown
    /// link) while still covering sequenced rows in between. Returns `None`
    /// when nothing is selected or no selected item has a sequence number.
    fn selected_seq_range(&self) -> Option<(usize, usize)> {
        let (start_idx, end_idx) = self.get_selected_index_range()?;
        let selected = self.app.transcript.get(start_idx..=end_idx)?;
        let min = selected.iter().filter_map(|item| item.seq()).min();
        let max = selected.iter().filter_map(|item| item.seq()).max();
        match (min, max) {
            (Some(from), Some(to)) => Some((from, to)),
            _ => None,
        }
    }

    /// Get text content from transcript item for copy/insert operations.
    pub(crate) fn get_transcript_item_text(item: &TranscriptItem) -> Option<String> {
        match item {
            TranscriptItem::UserText { text, .. } => Some(text.clone()),
            // Runtime notes copy/insert their raw text: the transcript shows
            // only the compact one-liner, so this is the only way to get the
            // full model-facing note into the clipboard or input box.
            TranscriptItem::RuntimeNote { text, .. } => Some(text.clone()),
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
            TranscriptItem::HandoffSession { agent, session_id } => {
                Some(format!("@ {} [{}]", agent, session_id))
            }
            TranscriptItem::UserSurface {
                surface_id,
                payload,
                ..
            } => Some(format!(
                "surface {surface_id}: {}",
                crate::surfaces::surface_headline(payload)
            )),
            _ => None,
        }
    }

    /// Handle 'e' key: open edit command for selected item(s).
    ///
    /// Editing only applies to user messages (the data layer rewrites user
    /// prompts), so a selection without one shows a footer notice instead of
    /// running a command that is guaranteed to fail. A range with exactly one
    /// user message proceeds normally: `.edit message <from>-<to>` rewrites
    /// only the user entry, leaving tool-round rows in the range untouched.
    pub(super) async fn handle_transcript_edit(&mut self) -> Result<()> {
        if self.refuse_when_pending_interaction() {
            // Read-only browsing while a confirmation is pending, even when
            // the session is idle (plan decision 6).
            return Ok(());
        }
        if self.app.llm_busy {
            self.show_busy_notice();
            return Ok(());
        }
        let Some(target) = self.actionable_transcript_binding() else {
            self.show_stale_transcript_notice();
            return Ok(());
        };
        let Some((start_idx, end_idx)) = self.get_selected_index_range() else {
            self.app.show_footer_notice("Nothing selected");
            return Ok(());
        };
        let Some(selected) = self.app.transcript.get(start_idx..=end_idx) else {
            self.app
                .show_footer_notice(&self.browse_refusal_noun("Can't edit this"));
            return Ok(());
        };
        match self.edit_outcome(selected) {
            EditOutcome::Notice(notice) => self.app.show_footer_notice(&notice),
            EditOutcome::Edit { from, to } => {
                let cmd = if from == to {
                    format!(".edit message {}", from)
                } else {
                    format!(".edit message {}-{}", from, to)
                };
                self.run_bound_history_command(&cmd, target).await?;
                self.app.transcript_focus = None;
                self.app.transcript_selection_anchor = None;
                self.app.transcript_browsing = false;
                self.app.scroll_state.follow = true;
            }
        }
        Ok(())
    }

    /// Decide what pressing `e` does with the selected rows: dispatch an
    /// `.edit message` command or refuse with a notice.
    fn edit_outcome(&self, selected: &[TranscriptItem]) -> EditOutcome {
        let user_messages = selected
            .iter()
            .filter(|item| matches!(item, TranscriptItem::UserText { seq: Some(_), .. }))
            .count();
        match (user_messages, self.selected_seq_range()) {
            // Nothing sequenced in the selection: nothing to address the edit
            // to. A sequenced selection without user messages can only hold
            // non-editable rows.
            (0, None) => EditOutcome::Notice(self.browse_refusal_noun("Can't edit this")),
            (0, Some(_)) => EditOutcome::Notice("Only user messages can be edited".to_string()),
            // Exactly one user message with its seq range: the edit runs.
            (1, Some((from, to))) => EditOutcome::Edit { from, to },
            (1, None) => EditOutcome::Notice(self.browse_refusal_noun("Can't edit this")),
            _ => EditOutcome::Notice("Edit supports a single user message at a time".to_string()),
        }
    }

    /// Handle 'd' or Delete key: open delete confirmation modal.
    pub(super) fn handle_transcript_delete(&mut self) {
        if self.refuse_when_pending_interaction() {
            // Read-only browsing while a confirmation is pending, even when
            // the session is idle (plan decision 6).
            return;
        }
        if self.app.llm_busy {
            // Mutating history while a turn runs races the worker: deleting an
            // in-flight ToolCalls round leaves results appending onto a stale
            // tail (#2557 review).
            self.show_busy_notice();
            return;
        }
        let Some(target) = self.actionable_transcript_binding() else {
            self.show_stale_transcript_notice();
            return;
        };
        let Some((from, to)) = self.selected_seq_range() else {
            self.app
                .show_footer_notice(&self.browse_refusal_noun("Can't delete this"));
            return;
        };
        self.app.modal = Some(crate::types::ModalState::ConfirmDelete { from, to, target });
    }

    /// Handle 'i' key: copy item text into input field, clear focus.
    fn handle_transcript_insert(&mut self) {
        if self.refuse_when_pending_interaction() {
            // Insert writes the hidden composer; refuse while a confirmation
            // is pending (plan decision 6).
            return;
        }
        self.insert_focused_transcript_text();
    }

    /// Copy the focused row's text into the composer and leave browsing.
    fn insert_focused_transcript_text(&mut self) {
        let Some(focus) = self.app.transcript_focus else {
            self.app.show_footer_notice("Nothing selected");
            return;
        };
        let item = match self.app.transcript.get(focus) {
            Some(item) => item.clone(),
            None => return,
        };
        let Some(text) = Self::get_transcript_item_text(&item) else {
            self.app.show_footer_notice("Nothing to insert");
            return;
        };
        self.set_input_text(&text);
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = false;
        self.app.scroll_state.follow = true;
    }

    /// Handle 'c' key: copy item text to clipboard.
    pub(super) fn handle_transcript_copy(&mut self) {
        let Some(focus) = self.app.transcript_focus else {
            self.app.show_footer_notice("Nothing selected");
            return;
        };
        match self
            .app
            .transcript
            .get(focus)
            .and_then(Self::get_transcript_item_text)
        {
            Some(text) => {
                match self.copy_text_to_clipboard(&text) {
                    Ok(()) => self.app.show_footer_notice("✓ Copied to clipboard"),
                    // The user must learn the copy failed — silently keeping
                    // the old notice or showing nothing is worse than either.
                    Err(_) => self.app.show_footer_notice("Copy failed"),
                }
            }
            None => self.app.show_footer_notice("Nothing to copy"),
        }
    }

    /// Copy to the clipboard, dispatching through the test seam when set.
    fn copy_text_to_clipboard(&self, text: &str) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(copy) = &self.copy_to_clipboard_override {
            return copy(text);
        }
        harnx_runtime::utils::set_text(text)
    }

    /// Handle 'r' key: open rewind confirmation modal.
    ///
    /// Always rewinds to the *earliest* selected item regardless of selection
    /// direction, so Shift+selecting up vs down yields the same target.
    pub(super) fn handle_transcript_rewind(&mut self) {
        if self.refuse_when_pending_interaction() {
            // Read-only browsing while a confirmation is pending, even when
            // the session is idle (plan decision 6).
            return;
        }
        if self.app.llm_busy {
            // Rewinding while a turn runs drops the round the worker is about
            // to append results for (#2557 review).
            self.show_busy_notice();
            return;
        }
        let Some(target) = self.actionable_transcript_binding() else {
            self.show_stale_transcript_notice();
            return;
        };
        // Browse mode can be active with no focus; degrade to a notice
        // instead of unwrapping.
        let Some(focus) = self.app.transcript_focus else {
            self.app.show_footer_notice("Nothing selected");
            return;
        };
        let focus = match self.app.transcript_selection_anchor {
            Some(anchor) => focus.min(anchor),
            None => focus,
        };
        let item = match self.app.transcript.get(focus) {
            Some(item) => item,
            None => return,
        };
        let Some(seq) = item.seq() else {
            self.app.show_footer_notice("Can't rewind to this item");
            return;
        };
        let user_text = match item {
            TranscriptItem::UserText { text, .. } => Some(text.clone()),
            _ => None,
        };
        self.app.modal = Some(crate::types::ModalState::ConfirmRewind {
            seq,
            user_text,
            target,
        });
    }
}

fn scroll_mouse_state(state: &mut ratatui_widget_scrolling::ScrollState, up: bool) {
    for _ in 0..3 {
        if up {
            state.scroll_up();
        } else {
            state.scroll_down();
        }
    }
}
