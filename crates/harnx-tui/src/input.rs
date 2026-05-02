use crate::render_helpers::{render_status_line, render_usage_line};
use crate::strip_ansi;
use crate::types::Tui;
use crate::types::{TranscriptItem, TuiEvent};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use harnx_core::event::{AgentEvent, AgentSource};
use harnx_render::pretty_error_string;
use harnx_runtime::config::{build_picker_context, sort_sessions_for_picker};
use harnx_runtime::utils::pretty_yaml_block;
use ratatui_textarea::{Input as TextInput, Key};
use std::path::Path;

const ATTACHMENT_PREVIEW_MAX_CHARS: usize = 800;
const ATTACHMENT_PREVIEW_MAX_LINES: usize = 12;

/// How long `start_prompt` waits for a prior prompt task to finish
/// cooperatively (after signalling its abort) before force-cancelling it
/// via `JoinHandle::abort`. Long enough for `bash_wait` and similar
/// cooperative tools to observe the abort and return; short enough that
/// the user does not feel a stall.
const PROMPT_TASK_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

fn unique_attachment_display_name(
    attachments: &[crate::types::Attachment],
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

fn render_attachment_preview(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let mut lines = Vec::new();
    let mut chars_seen = 0usize;

    for line in text.lines().take(ATTACHMENT_PREVIEW_MAX_LINES) {
        if chars_seen >= ATTACHMENT_PREVIEW_MAX_CHARS {
            break;
        }
        let remaining = ATTACHMENT_PREVIEW_MAX_CHARS.saturating_sub(chars_seen);
        let snippet: String = line.chars().take(remaining).collect();
        chars_seen += snippet.chars().count();
        lines.push(snippet);
    }

    if lines.is_empty() {
        return None;
    }

    let truncated = text.chars().count() > chars_seen || text.lines().count() > lines.len();
    let mut preview = lines.join("\n");
    if truncated {
        preview.push_str("\n...");
    }
    Some(preview)
}

/// Build the body for a `TranscriptItem::ToolCall` from a `Started`
/// event's `markdown` and `input`. A non-empty rendered template `markdown`
/// becomes `ToolCallBody::Markdown`; otherwise the raw input is YAML-
/// formatted (or omitted entirely when input is `null`).
fn tool_call_body(
    markdown: Option<&str>,
    input: &serde_json::Value,
) -> Option<crate::types::ToolCallBody> {
    match markdown.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => Some(crate::types::ToolCallBody::Markdown(t.to_string())),
        None => match input {
            serde_json::Value::Null => None,
            _ => Some(crate::types::ToolCallBody::Yaml(pretty_yaml_block(input))),
        },
    }
}

/// Convert a `Completed` event's `output` + `markdown` into transcript items.
/// The whole multi-line text is wrapped in a single `ToolResultMarkdown`
/// item so `markdown_lines` can parse block-level constructs — fenced
/// code (e.g. the ```diff blocks emitted by harnx-mcp-fs / harnx-mcp-bash
/// for history diffs), inline emphasis from a templated MCP
/// `result_template`, and plain text alike. Strips ANSI escapes from
/// string outputs before extraction so pre-dimmed test inputs render
/// cleanly.
fn tool_completed_to_transcript_items(
    output: &serde_json::Value,
    markdown: Option<&str>,
) -> Vec<TranscriptItem> {
    let raw = match output {
        serde_json::Value::String(s) => serde_json::Value::String(strip_ansi(s)),
        _ => output.clone(),
    };
    let text = crate::agent_event_sink::render_tool_result_text(&raw, markdown);
    let clean = strip_ansi(&text).trim_end_matches('\n').to_string();
    if clean.is_empty() {
        return vec![];
    }
    vec![TranscriptItem::ToolResultMarkdown(clean)]
}

impl Tui {
    pub(super) async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // If a modal is open, intercept all keys and route to modal handler
        if self.app.modal.is_some() {
            return self.handle_modal_key(key).await;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.abort_signal.set_ctrld();
                self.app.should_quit = true;
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // Abort signal goes both to the Tui-level signal (used by
                // dot-commands) and to the in-flight prompt task's own
                // signal (if any). Per-task signals are why we no longer
                // need to "reset" anything before the next submission —
                // the running task can never be un-aborted.
                self.abort_signal.set_ctrlc();
                if let Some(prompt_abort) = &self.current_prompt_abort {
                    prompt_abort.set_ctrlc();
                }
                self.app.transcript.push(TranscriptItem::SystemText(
                    "(Ctrl+C — operation aborted. Ctrl+D to exit.)".to_string(),
                ));
                // Discard any queued message — Ctrl+C means "cancel
                // everything", including the message you typed while the
                // task was running.
                self.app.pending_message = None;
                *self.shared_pending_message.lock().await = None;
                // `llm_busy` stays true while a prompt task is still
                // winding down; the Final/Error event from that task is
                // what flips it off. Flipping it eagerly here is what
                // produced Bug 2 — the next Enter would race a fresh
                // prompt task against the still-running old one. When no
                // prompt task is in flight (idle Ctrl+C) we still clear
                // the flag for parity with the prior UX.
                if self.current_prompt_handle.is_none() {
                    self.app.llm_busy = false;
                }
            }
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
                if self.app.transcript_focus.is_some() {
                    self.app.transcript_focus = None;
                    self.app.transcript_selection_anchor = None;
                } else if !self.app.completions.is_empty() {
                    self.app.completions.clear();
                }
            }
            // D4: Keyboard actions on selected transcript item(s)
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
                self.app.action_menu_open = true;
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
                        // Queue the message to send when LLM finishes or
                        // after the next tool round completes.
                        // Keep the text in input so user can see/edit it.
                        let pending_attachments = self.app.attachments.clone();
                        let pending_attachment_dir = self.app.attachment_dir.clone();
                        let pending = crate::types::PendingMessage {
                            text,
                            attachments: pending_attachments,
                            attachment_dir: pending_attachment_dir,
                            paste_count: self.app.paste_count,
                        };
                        self.app.pending_message = Some(pending.clone());
                        // Publish to shared state so the prompt task can
                        // pick it up between tool rounds.
                        *self.shared_pending_message.lock().await = Some(pending);
                        self.refresh_input_chrome();
                    } else if text.trim_start().starts_with('.') {
                        // Dot-command: route through command handler
                        let attachments_snapshot = self.app.attachments.clone();
                        self.app.transcript.push(TranscriptItem::UserText {
                            text: text.clone(),
                            seq: None,
                            timestamp: Some(chrono::Utc::now()),
                        });
                        self.render_submitted_attachments(&attachments_snapshot);
                        self.pin_transcript_to_bottom();
                        self.app.input = Self::new_input();
                        self.run_command(&text).await?;
                        self.refresh_input_chrome();
                    } else {
                        let attachments_snapshot = self.app.attachments.clone();
                        self.app.transcript.push(TranscriptItem::UserText {
                            text: text.clone(),
                            seq: None,
                            timestamp: Some(chrono::Utc::now()),
                        });
                        self.render_submitted_attachments(&attachments_snapshot);
                        self.pin_transcript_to_bottom();
                        self.app.input = Self::new_input();
                        let msg = crate::types::PendingMessage {
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
                    self.clear_shared_pending_message().await;
                    self.refresh_input_chrome();
                }
                self.app.input.input(TextInput {
                    key: Key::Enter,
                    ..Default::default()
                });
            }
            _ => {
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

    /// Ensure the attachment temp directory exists, creating it via mkdtemp if needed.
    async fn ensure_attachment_dir(&mut self) -> std::io::Result<std::path::PathBuf> {
        if let Some(ref dir) = self.app.attachment_dir {
            Ok(dir.clone())
        } else {
            let dir = crate::types::create_attachment_dir()?;
            self.app.attachment_dir = Some(dir.clone());
            Ok(dir)
        }
    }

    /// Clean up the attachment temp directory and reset attachment state.
    pub(super) fn cleanup_attachments(&mut self) {
        self.app.attachments.clear();
        if let Some(dir) = self.app.attachment_dir.take() {
            crate::types::cleanup_attachment_dir(&dir);
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
                            self.app.transcript.push(TranscriptItem::ErrorText(format!(
                                "Failed to copy attachment: {err}"
                            )));
                        } else {
                            self.app.attachments.push(crate::types::Attachment {
                                path: dest,
                                display_name,
                            });
                        }
                    }
                    Err(err) => {
                        self.app.transcript.push(TranscriptItem::ErrorText(format!(
                            "Failed to create attachment directory: {err}"
                        )));
                    }
                }
            } else {
                self.app.transcript.push(TranscriptItem::ErrorText(format!(
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
                    self.app.transcript.push(TranscriptItem::ErrorText(format!(
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
            self.app.paste_count = pending.paste_count;
            self.clear_shared_pending_message().await;
            self.refresh_input_chrome();
        }
        // Exit history preview on paste — keep current content as new draft
        if self.app.history_preview {
            self.app.history_index = None;
            self.app.history_preview = false;
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
                    self.app.transcript.push(TranscriptItem::ErrorText(format!(
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
    ) -> std::io::Result<crate::types::Attachment> {
        let dir = self.ensure_attachment_dir().await?;
        self.app.paste_count += 1;
        let filename = format!("paste-{}.txt", self.app.paste_count);
        let path = dir.join(&filename);
        tokio::fs::write(&path, text).await?;
        Ok(crate::types::Attachment {
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

    pub(crate) async fn handle_tui_event(&mut self, event: TuiEvent) -> Result<()> {
        self.handle_tui_event_inner(event).await
    }

    async fn handle_tui_event_inner(&mut self, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Agent(event, source) => {
                self.render_agent_event(event, source).await;
            }
            TuiEvent::ToolRoundComplete => {
                // Intermediate tool round — prompt loop continues, don't clear llm_busy.
                // Flush any pending thought so follow-up thought after tool results
                // starts a fresh block instead of appending to the earlier one.
                self.flush_pending_thought();
                // Reset streaming index so the next LLM turn creates a fresh
                // AssistantText item instead of appending to the previous one.
                // This keeps tool-call rows visually between the two turns.
                self.app.streaming_assistant_idx = None;
                self.pin_transcript_to_bottom();
            }
            TuiEvent::PendingMessageConsumed(pending) => {
                // The prompt task consumed our pending message during a tool
                // round.  Clear the local pending state, reset the input field,
                // and show the consumed text (and any attachments) in the
                // transcript.
                self.app.pending_message = None;
                self.app.input = Self::new_input();
                self.app.transcript.push(TranscriptItem::UserText {
                    text: pending.text.clone(),
                    seq: None,
                    timestamp: Some(chrono::Utc::now()),
                });
                self.render_submitted_attachments(&pending.attachments);
                self.pin_transcript_to_bottom();
                self.refresh_input_chrome();
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn submit_pending_message(
        &mut self,
        pending: crate::types::PendingMessage,
    ) -> Result<()> {
        self.submit_pending_message_inner(pending).await
    }

    #[cfg(not(test))]
    async fn submit_pending_message(
        &mut self,
        pending: crate::types::PendingMessage,
    ) -> Result<()> {
        self.submit_pending_message_inner(pending).await
    }

    async fn submit_pending_message_inner(
        &mut self,
        pending: crate::types::PendingMessage,
    ) -> Result<()> {
        self.app.input = Self::new_input();
        self.app.transcript.push(TranscriptItem::UserText {
            text: pending.text.clone(),
            seq: None,
            timestamp: Some(chrono::Utc::now()),
        });
        self.render_submitted_attachments(&pending.attachments);
        self.pin_transcript_to_bottom();
        if pending.text.trim_start().starts_with('.') {
            self.app.attachments = pending.attachments;
            self.app.attachment_dir = pending.attachment_dir;
            self.app.paste_count = pending.paste_count;
            self.run_command(&pending.text).await?;
            self.refresh_input_chrome();
        } else {
            self.start_prompt(pending).await?;
        }
        Ok(())
    }

    /// Clear the shared pending message so the prompt task does not consume a
    /// stale value after the user cancels or edits the pending draft.
    async fn clear_shared_pending_message(&self) {
        *self.shared_pending_message.lock().await = None;
    }

    fn render_submitted_attachments(&mut self, attachments: &[crate::types::Attachment]) {
        if attachments.is_empty() {
            return;
        }

        self.app
            .transcript
            .push(TranscriptItem::AttachmentHeader(format!(
                "Attachments ({})",
                attachments.len()
            )));

        for attachment in attachments {
            self.app.transcript.push(TranscriptItem::AttachmentItem(
                attachment.display_name.clone(),
            ));

            if let Some(preview) = render_attachment_preview(&attachment.path) {
                for line in preview.lines() {
                    self.app
                        .transcript
                        .push(TranscriptItem::AttachmentPreviewLine(line.to_string()));
                }
            }
        }
    }

    fn flush_pending_thought(&mut self) {
        if self.app.pending_thought_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.app.pending_thought_text);
        self.app.pending_thought_source = None;
        self.app
            .transcript
            .push(TranscriptItem::ThoughtText(text.trim().to_string()));
    }

    #[cfg(test)]
    pub(crate) fn flush_pending_thought_for_test(&mut self) {
        self.flush_pending_thought();
    }

    async fn render_agent_event(&mut self, event: AgentEvent, source: Option<AgentSource>) {
        use harnx_core::event::{ModelEvent, NoticeEvent, SessionEvent, ToolEvent};

        let is_thought = matches!(&event, AgentEvent::Model(ModelEvent::ThoughtChunk { .. }));
        let is_usage = matches!(&event, AgentEvent::Model(ModelEvent::Usage { .. }));
        // Tool calls (and plan updates) from sub-agents represent a turn-like
        // boundary in the sub-agent's output: text streamed *before* the tool
        // call belongs visually above it, text streamed *after* belongs
        // visually below.  Reset the streaming-assistant index here so a
        // subsequent MessageChunk starts a fresh AssistantText rather than
        // being appended to the text that preceded the tool call.  We
        // deliberately do NOT reset for display-only events (Notice::Info,
        // Tool::Completed, Model::Usage, Tool::Update) so that LLM text
        // chunks with trailing-newline completion still coalesce correctly
        // across those events.
        let is_turn_boundary = matches!(
            &event,
            AgentEvent::Tool(ToolEvent::Started { .. }) | AgentEvent::Plan { .. }
        );
        if is_turn_boundary {
            self.app.streaming_assistant_idx = None;
        }
        // Handle LogSeqAssigned before any heading/thought side-effects — it
        // is a pure seq-assignment event and should not create stray headings
        // or flush pending thoughts.
        if let AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) = event {
            for item in self.app.transcript.iter_mut().rev() {
                match item {
                    TranscriptItem::UserText {
                        seq: item_seq @ None,
                        ..
                    }
                    | TranscriptItem::AssistantText {
                        seq: item_seq @ None,
                        ..
                    }
                    | TranscriptItem::ToolCall {
                        seq: item_seq @ None,
                        ..
                    } => {
                        *item_seq = Some(seq);
                        break;
                    }
                    _ => {}
                }
            }
            return;
        }

        if !is_thought {
            self.flush_pending_thought();
        }
        self.render_ui_output_heading(source.as_ref(), is_usage);

        let rendered_entries = match event {
            AgentEvent::Notice(NoticeEvent::Info(text)) => {
                let clean = strip_ansi(&text).trim_end_matches('\n').to_string();
                if clean.is_empty() {
                    vec![]
                } else {
                    vec![TranscriptItem::SystemText(clean)]
                }
            }
            AgentEvent::Notice(NoticeEvent::Warning(msg)) => {
                let text = format!("⚠ {msg}");
                let clean = strip_ansi(&text).trim_end_matches('\n').to_string();
                if clean.is_empty() {
                    vec![]
                } else {
                    vec![TranscriptItem::SystemText(clean)]
                }
            }
            AgentEvent::Notice(NoticeEvent::Error(msg)) => {
                let text = format!("error: {msg}");
                let clean = strip_ansi(&text).trim_end_matches('\n').to_string();
                if clean.is_empty() {
                    vec![]
                } else {
                    vec![TranscriptItem::SystemText(clean)]
                }
            }
            AgentEvent::Tool(ToolEvent::Completed {
                output, markdown, ..
            }) => tool_completed_to_transcript_items(&output, markdown.as_deref()),
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                let text = concat_text_blocks(&blocks);
                if text.is_empty() {
                    vec![]
                } else {
                    self.append_streaming_assistant_chunk(&text);
                    self.pin_transcript_to_bottom();
                    vec![]
                }
            }
            AgentEvent::Model(ModelEvent::Final { output, usage }) => {
                self.flush_pending_thought();
                self.app.llm_busy = false;
                // The task that emitted Final has signalled it is exiting.
                // Drop our reference to its abort signal — the next Ctrl+C
                // should not target a task that's already gone. We keep
                // the JoinHandle until the next `start_prompt` so the
                // drain step has something to await on (already-completed
                // handles resolve immediately).
                self.current_prompt_abort = None;
                // Defensive cleanup of the pending-message channel: the
                // normal mid-tool-round consumption already clears it,
                // but a text-only response path leaves it set, where it
                // would otherwise leak into the NEXT prompt task and be
                // re-injected as a duplicate user message.
                *self.shared_pending_message.lock().await = None;
                self.app.last_ui_output_source = None;
                let usage_str = format_usage(&usage);
                if !output.is_empty() {
                    if let Some(idx) = self.app.streaming_assistant_idx {
                        match self.app.transcript.get_mut(idx) {
                            Some(TranscriptItem::AssistantText { text: existing, .. })
                                if !existing.is_empty() =>
                            {
                                if existing != &output {
                                    *existing = output;
                                }
                            }
                            _ => {
                                self.app.transcript.push(TranscriptItem::AssistantText {
                                    text: output,
                                    seq: None,
                                    timestamp: Some(chrono::Utc::now()),
                                });
                                self.app.streaming_assistant_idx =
                                    Some(self.app.transcript.len() - 1);
                            }
                        }
                    } else {
                        self.app.transcript.push(TranscriptItem::AssistantText {
                            text: output,
                            seq: None,
                            timestamp: Some(chrono::Utc::now()),
                        });
                        self.app.streaming_assistant_idx = Some(self.app.transcript.len() - 1);
                    }
                    self.pin_transcript_to_bottom();
                }
                self.app.streaming_assistant_idx = None;
                if !usage_str.is_empty() {
                    self.app
                        .transcript
                        .push(TranscriptItem::SystemText(format!("Usage: {usage_str}")));
                    self.pin_transcript_to_bottom();
                }
                self.refresh_input_chrome();

                if let Some(pending) = self.app.pending_message.take() {
                    if let Err(err) = self.submit_pending_message(pending).await {
                        self.app
                            .transcript
                            .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
                        self.pin_transcript_to_bottom();
                    }
                }
                vec![]
            }
            AgentEvent::Model(ModelEvent::Error(err)) => {
                self.flush_pending_thought();
                self.app.llm_busy = false;
                // Mirrors the Final cleanup: drop the per-task abort
                // signal (task has exited) and clear the shared pending
                // channel so its content can't leak into the next task.
                self.current_prompt_abort = None;
                *self.shared_pending_message.lock().await = None;
                self.app.streaming_assistant_idx = None;
                self.app.last_ui_output_source = None;
                self.app.transcript.push(TranscriptItem::ErrorText(err));
                self.pin_transcript_to_bottom();
                self.refresh_input_chrome();

                if let Some(pending) = self.app.pending_message.take() {
                    if let Err(err) = self.submit_pending_message(pending).await {
                        self.app
                            .transcript
                            .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
                        self.pin_transcript_to_bottom();
                    }
                }
                vec![]
            }
            AgentEvent::Model(ModelEvent::ThoughtChunk { blocks }) => {
                let text = concat_text_blocks(&blocks);
                let clean = strip_ansi(&text)
                    .trim_start_matches("<think>")
                    .trim_end_matches("</think>")
                    .to_string();
                if clean.trim().is_empty() {
                    vec![]
                } else {
                    if self.app.pending_thought_source != source {
                        self.flush_pending_thought();
                        self.app.pending_thought_source = source.clone();
                    }
                    self.app.pending_thought_text.push_str(&clean);
                    vec![]
                }
            }
            AgentEvent::Tool(ToolEvent::Update {
                markdown, status, ..
            }) => {
                let status_str = status.map(|s| format!("{s:?}").to_lowercase());
                if let Some(text) = render_status_line(markdown.as_deref(), status_str.as_deref()) {
                    vec![TranscriptItem::StatusLine(text)]
                } else {
                    vec![]
                }
            }
            AgentEvent::Plan { entries } => vec![TranscriptItem::Plan(entries)],
            AgentEvent::Model(ModelEvent::Usage {
                input,
                output,
                cached,
                session_label,
            }) => {
                let line = render_usage_line(
                    input,
                    output,
                    cached,
                    session_label.as_deref(),
                    source.as_ref(),
                );
                if let Some(line) = line {
                    if self.update_existing_usage_line(source.as_ref(), &line) {
                        vec![]
                    } else {
                        vec![TranscriptItem::UsageLine(line)]
                    }
                } else {
                    vec![]
                }
            }
            AgentEvent::Tool(ToolEvent::Started {
                name,
                markdown,
                input,
                ..
            }) => {
                vec![TranscriptItem::ToolCall {
                    tool_name: name,
                    body: tool_call_body(markdown.as_deref(), &input),
                    seq: None,
                    timestamp: Some(chrono::Utc::now()),
                }]
            }
            // Not rendered by the TUI: Turn, Session, Status, Tool::Progress,
            // Tool::Failed.
            _ => vec![],
        };

        if !rendered_entries.is_empty() {
            let start_idx = self.app.transcript.len();
            self.app.transcript.extend(rendered_entries);
            if is_usage {
                self.app.last_usage_source = source.clone();
                self.app.last_usage_transcript_idx = Some(start_idx);
            } else {
                self.clear_usage_tracking();
            }
            self.pin_transcript_to_bottom();
        } else if is_thought || is_usage {
            self.pin_transcript_to_bottom();
        }
    }

    fn render_ui_output_heading(&mut self, source: Option<&AgentSource>, is_usage: bool) {
        let source = source.cloned();
        if source != self.app.last_ui_output_source {
            if let Some(source) = &source {
                self.app
                    .transcript
                    .push(TranscriptItem::SourceHeading(source.clone()));
            }
            self.app.last_ui_output_source = source;
            // Reset streaming-assistant tracking: a source change means the
            // next MessageChunk event belongs to a different agent than
            // whatever the previous AssistantText entry was aggregating, so
            // it must start a new AssistantText entry (rendered below the
            // just-inserted SourceHeading) rather than being appended to the
            // previous agent's text.  Without this reset, sub-agent message
            // chunks get concatenated onto the parent's AssistantText,
            // producing a single run-on paragraph that mixes content from
            // multiple agents on the top-level row.
            self.app.streaming_assistant_idx = None;
        }
        if !is_usage {
            self.clear_usage_tracking();
        }
    }

    fn clear_usage_tracking(&mut self) {
        self.app.last_usage_source = None;
        self.app.last_usage_transcript_idx = None;
    }

    fn update_existing_usage_line(&mut self, source: Option<&AgentSource>, line: &str) -> bool {
        if self.app.last_usage_source.as_ref() != source {
            return false;
        }
        let Some(idx) = self.app.last_usage_transcript_idx else {
            return false;
        };
        let Some(entry) = self.app.transcript.get_mut(idx) else {
            self.clear_usage_tracking();
            return false;
        };
        match entry {
            TranscriptItem::UsageLine(existing) => {
                *existing = line.to_string();
                true
            }
            _ => {
                self.clear_usage_tracking();
                false
            }
        }
    }

    pub(super) async fn start_prompt(&mut self, msg: crate::types::PendingMessage) -> Result<()> {
        // Drain any prior prompt task BEFORE spawning the new one. Two
        // prompt tasks must never run concurrently against the same
        // session — they would interleave save_session_tool_calls /
        // save_session_tool_results writes and corrupt the in-memory
        // pending Tool message (see Bug 2: orphan tool_calls in the
        // session log around line 24785/24794 of the reproducing
        // session).
        self.drain_previous_prompt_task().await;

        // Allocate a fresh abort signal for this task. Subsequent Ctrl+C
        // will signal exactly this task; later submissions get their
        // own fresh signal so that nothing in this branch can be
        // un-aborted by a future `abort_signal.reset()`.
        let new_abort = harnx_runtime::utils::create_abort_signal();
        self.current_prompt_abort = Some(new_abort.clone());

        self.app.llm_busy = true;

        let event_tx = self.event_tx.clone();
        let ctx = crate::prompt::PromptTaskContext {
            config: self.config.clone(),
            abort_signal: new_abort,
            async_manager: self.async_manager.clone(),
            persistent_manager: self.persistent_manager.clone(),
            pending_async_context: self.pending_async_context.clone(),
            shared_pending_message: self.shared_pending_message.clone(),
            event_tx: event_tx.clone(),
        };

        let handle = tokio::spawn(async move {
            let result: Result<()> = Self::run_prompt_task(msg, ctx).await;
            if let Err(err) = result {
                use harnx_core::event::{AgentEvent, ModelEvent};
                let _ = event_tx.send(TuiEvent::Agent(
                    AgentEvent::Model(ModelEvent::Error(pretty_error_string(&err))),
                    None,
                ));
            }
        });
        self.current_prompt_handle = Some(handle);

        Ok(())
    }

    /// Wait for any prior prompt task to finish before spawning a new
    /// one. Cooperative shutdown via the prior task's abort signal is
    /// tried first with a short timeout; if the task does not exit
    /// within `PROMPT_TASK_DRAIN_TIMEOUT`, force-cancel it via
    /// `JoinHandle::abort`.
    async fn drain_previous_prompt_task(&mut self) {
        // Signal cooperative abort first (if a signal is around). This is
        // a no-op if the prior task has already finished and we just
        // never cleared the signal.
        if let Some(abort) = self.current_prompt_abort.take() {
            abort.set_ctrlc();
        }

        let Some(handle) = self.current_prompt_handle.take() else {
            return;
        };

        // Already-completed handle resolves immediately; live handle is
        // given up to PROMPT_TASK_DRAIN_TIMEOUT to wind down before we
        // hard-cancel it.
        let abort_handle = handle.abort_handle();
        match tokio::time::timeout(PROMPT_TASK_DRAIN_TIMEOUT, handle).await {
            Ok(Ok(())) => {} // task ended cleanly
            Ok(Err(_)) => {
                // Task panicked or was already cancelled; the unwound
                // task can no longer touch session state, so we move on.
            }
            Err(_) => {
                // Cooperative shutdown timed out — force the task to
                // stop. The corresponding future is dropped at its next
                // .await; until then it's wedged on something
                // synchronous (block_in_place / a non-cooperative tool).
                abort_handle.abort();
            }
        }
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

    fn handle_up_key(&mut self, key: KeyEvent) {
        if !self.app.completions.is_empty() {
            self.app.scroll_state.scroll_up();
        } else if let Some(focus) = self.app.transcript_focus {
            if focus > 0 {
                self.app.transcript_focus = Some(focus - 1);
                self.app.transcript_selection_anchor = None;
            } else {
                self.app.transcript_focus = None;
                self.app.transcript_selection_anchor = None;

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
            self.app.transcript_focus = Some(self.app.transcript.len() - 1);
            self.app.transcript_selection_anchor = None;
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
        if !self.app.completions.is_empty() {
            self.app.scroll_state.scroll_down();
        } else if let Some(focus) = self.app.transcript_focus {
            let next = focus + 1;
            if next < self.app.transcript.len() {
                self.app.transcript_focus = Some(next);
                self.app.transcript_selection_anchor = None;
            } else {
                self.app.transcript_focus = None;
                self.app.transcript_selection_anchor = None;
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
        if let Some(focus) = self.app.transcript_focus {
            if self.app.transcript_selection_anchor.is_none() {
                self.app.transcript_selection_anchor = Some(focus);
            }
            if focus > 0 {
                self.app.transcript_focus = Some(focus - 1);
            }
        }
    }

    fn handle_down_key_shift(&mut self) {
        if let Some(focus) = self.app.transcript_focus {
            if self.app.transcript_selection_anchor.is_none() {
                self.app.transcript_selection_anchor = Some(focus);
            }
            let next = focus + 1;
            if next < self.app.transcript.len() {
                self.app.transcript_focus = Some(next);
            }
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
            let commands: Vec<(String, Option<String>)> = harnx_runtime::commands::COMMANDS
                .iter()
                .filter(|c| c.name.starts_with(filter))
                .map(|c| (format!("{} ", c.name), Some(c.description.to_string())))
                .collect();
            return commands;
        }

        // For multi-part commands, delegate to config's command_complete
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
            return self.config.read().command_complete(cmd, &args, filter);
        }

        vec![]
    }

    fn reconcile_transcript_after_command(
        &mut self,
        prev_session: Option<String>,
        prev_agent: Option<String>,
        command_was: &str,
    ) {
        let (curr_session, curr_agent) = {
            let cfg = self.config.read();
            let s = cfg.session.as_ref().map(|s| s.name().to_string());
            let a = cfg.agent.as_ref().map(|a| a.name().to_string());
            (s, a)
        };

        let needs_reconcile = curr_session != prev_session
            || curr_agent != prev_agent
            || command_was.starts_with(".empty session")
            || command_was.starts_with(".reset session")
            || command_was.starts_with(".reset repl")
            || command_was.starts_with(".compact session")
            || command_was.starts_with(".edit session")
            || command_was.starts_with(".edit message ")
            || command_was.starts_with(".delete message ")
            || command_was.starts_with(".rewind ");

        if !needs_reconcile {
            return;
        }

        self.app.transcript.clear();
        self.app.streaming_assistant_idx = None;
        // Reset scroll state so the widget doesn't subtract-overflow when
        // the rebuilt transcript is shorter than the previous one.
        self.app.scroll_state = ratatui_widget_scrolling::ScrollState::new();
        self.app.transcript = Self::build_initial_transcript(&self.config);
        self.pin_transcript_to_bottom();
    }

    pub(super) async fn run_command(&mut self, line: &str) -> Result<()> {
        let prev_session = self
            .config
            .read()
            .session
            .as_ref()
            .map(|s| s.name().to_string());
        let prev_agent = self
            .config
            .read()
            .agent
            .as_ref()
            .map(|a| a.name().to_string());
        // Run the command inside a block that owns the lock guards so they are
        // dropped before we touch `self` again for transcript / UI updates.
        let (result, captured) = {
            let config = self.config.clone();
            let abort_signal = self.abort_signal.clone();
            let mut async_manager = self.async_manager.lock().await;
            let mut pending_async_context = self.pending_async_context.lock().await;
            let mut output = Vec::<u8>::new();

            let result = harnx_runtime::commands::run_command_with_output(
                &config,
                abort_signal,
                line,
                &mut async_manager,
                &self.persistent_manager,
                &mut pending_async_context,
                &mut output,
            )
            .await;

            let captured = String::from_utf8_lossy(&output).into_owned();
            (result, captured)
            // async_manager + pending_async_context guards drop here
        };

        let clean = strip_ansi(&captured).trim_end_matches('\n').to_string();
        let line_cmd = line.trim_start();
        let is_mutation_command = line_cmd.starts_with(".edit message ")
            || line_cmd.starts_with(".delete message ")
            || line_cmd.starts_with(".rewind ");

        match result {
            Ok(outcome) => {
                if matches!(outcome, harnx_runtime::commands::CommandOutcome::Exit) {
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
                self.reconcile_transcript_after_command(prev_session, prev_agent, line);
                if !clean.is_empty() {
                    if is_mutation_command {
                        self.app
                            .transcript
                            .push(TranscriptItem::MutationNotice(clean.clone()));
                    } else {
                        self.app
                            .transcript
                            .push(TranscriptItem::SystemText(clean.clone()));
                    }
                    self.pin_transcript_to_bottom();
                }
            }
            Err(err) => {
                self.app
                    .transcript
                    .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
            }
        }
        Ok(())
    }
}

/// Concatenate `ContentBlock::Text(..)` fragments into a single String.
/// Non-Text blocks (Image, ResourceLink, Opaque) are skipped — the TUI
/// transcript currently only renders text.
fn concat_text_blocks(blocks: &[harnx_core::event::ContentBlock]) -> String {
    use harnx_core::event::ContentBlock;
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text(t) = block {
            out.push_str(t);
        }
    }
    out
}

/// Reproduce the textual representation of `CompletionTokenUsage` that the
/// legacy `UiOutputEventKind::LlmFinal { usage: CompletionTokenUsage }` path
/// produced. Pre-migration the TUI tested `!usage.is_empty()` (input==0 &&
/// output==0) and then formatted via `format!("Usage: {usage}")` using the
/// Display impl. Mirror that contract: return empty when `is_empty()`, else
/// the Display output. Callers then test `!usage_str.is_empty()` to decide
/// whether to emit a `Usage:` transcript line.
fn format_usage(usage: &harnx_core::api_types::CompletionTokenUsage) -> String {
    if usage.is_empty() {
        String::new()
    } else {
        format!("{usage}")
    }
}

impl Tui {
    /// Handle keystrokes while a confirmation modal is open.
    ///
    /// - `y` or `Enter` → confirm action, clear modal, execute the action.
    /// - `n` or `Esc` → cancel, clear modal.
    /// - All other keys are consumed (no action).
    pub(super) async fn handle_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        match self.app.modal.as_ref() {
            Some(crate::types::ModalState::AgentPicker { .. })
            | Some(crate::types::ModalState::SessionPicker { .. }) => {
                self.handle_picker_key(key).await?;
            }
            Some(_) => match (key.code, key.modifiers) {
                (KeyCode::Char('y'), KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::NONE) => {
                    self.confirm_modal_action().await?;
                }
                (KeyCode::Char('n'), KeyModifiers::NONE) | (KeyCode::Esc, KeyModifiers::NONE) => {
                    self.app.modal = None;
                }
                _ => {}
            },
            None => {}
        }
        Ok(())
    }

    async fn handle_picker_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up => {
                if let Some(crate::types::ModalState::AgentPicker { selected, .. })
                | Some(crate::types::ModalState::SessionPicker { selected, .. }) =
                    self.app.modal.as_mut()
                {
                    *selected = selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(crate::types::ModalState::AgentPicker { selected, agents }) =
                    self.app.modal.as_mut()
                {
                    if *selected + 1 < agents.len() {
                        *selected += 1;
                    }
                } else if let Some(crate::types::ModalState::SessionPicker { selected, sessions }) =
                    self.app.modal.as_mut()
                {
                    if *selected + 1 < sessions.len() {
                        *selected += 1;
                    }
                }
            }
            KeyCode::Enter => {
                let modal = self.app.modal.take();
                match modal {
                    Some(crate::types::ModalState::AgentPicker { agents, selected }) => {
                        if selected < agents.len() {
                            let agent_name = agents[selected].clone();
                            let prev_session = self
                                .config
                                .read()
                                .session
                                .as_ref()
                                .map(|s| s.name().to_string());
                            let prev_agent = self
                                .config
                                .read()
                                .agent
                                .as_ref()
                                .map(|a| a.name().to_string());

                            self.config.write().use_agent_by_name(&agent_name)?;

                            let sessions: Vec<_> = self
                                .config
                                .read()
                                .list_sessions_with_meta()
                                .into_iter()
                                .filter(|s| s.agent_name.as_deref() == Some(agent_name.as_str()))
                                .collect();
                            if !sessions.is_empty() {
                                let ctx = build_picker_context();
                                let sessions = sort_sessions_for_picker(sessions, &ctx);
                                self.app.modal = Some(crate::types::ModalState::SessionPicker {
                                    sessions,
                                    selected: 0,
                                });
                            } else {
                                self.config.write().use_session(None)?;
                                let llm_busy = self.app.llm_busy;
                                let pending = self.app.pending_message.is_some();
                                Self::refresh_input_chrome_from_state(
                                    &self.config,
                                    &mut self.app,
                                    llm_busy,
                                    pending,
                                );
                                self.reconcile_transcript_after_command(
                                    prev_session,
                                    prev_agent,
                                    ".agent",
                                );
                            }
                        }
                    }
                    Some(crate::types::ModalState::SessionPicker { sessions, selected }) => {
                        if selected < sessions.len() {
                            let session_name = sessions[selected].name.clone();
                            let prev_session = self
                                .config
                                .read()
                                .session
                                .as_ref()
                                .map(|s| s.name().to_string());
                            let prev_agent = self
                                .config
                                .read()
                                .agent
                                .as_ref()
                                .map(|a| a.name().to_string());

                            self.config.write().use_session(Some(&session_name))?;

                            let llm_busy = self.app.llm_busy;
                            let pending = self.app.pending_message.is_some();
                            Self::refresh_input_chrome_from_state(
                                &self.config,
                                &mut self.app,
                                llm_busy,
                                pending,
                            );
                            self.reconcile_transcript_after_command(
                                prev_session,
                                prev_agent,
                                ".session",
                            );
                        }
                    }
                    _ => {
                        self.app.modal = modal;
                    }
                }
            }
            KeyCode::Esc => {
                let modal = self.app.modal.take();
                if let Some(crate::types::ModalState::SessionPicker { .. }) = modal {
                    let prev_session = self
                        .config
                        .read()
                        .session
                        .as_ref()
                        .map(|s| s.name().to_string());
                    let prev_agent = self
                        .config
                        .read()
                        .agent
                        .as_ref()
                        .map(|a| a.name().to_string());
                    self.config.write().use_session(None)?;

                    let llm_busy = self.app.llm_busy;
                    let pending = self.app.pending_message.is_some();
                    Self::refresh_input_chrome_from_state(
                        &self.config,
                        &mut self.app,
                        llm_busy,
                        pending,
                    );
                    self.reconcile_transcript_after_command(prev_session, prev_agent, ".session");
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Execute the action associated with the current modal and clear it.
    async fn confirm_modal_action(&mut self) -> Result<()> {
        let modal = self.app.modal.take();
        if let Some(modal) = modal {
            match modal {
                crate::types::ModalState::ConfirmDelete { from, to } => {
                    // Execute delete via dot-command
                    let cmd = if from == to {
                        format!(".delete message {}", from)
                    } else {
                        format!(".delete message {}-{}", from, to)
                    };
                    self.run_command(&cmd).await?;
                }
                crate::types::ModalState::ConfirmRewind { seq, user_text } => {
                    // Execute rewind via dot-command
                    let cmd = format!(".rewind {}", seq);
                    self.run_command(&cmd).await?;
                    // If user_text was saved, restore it to the input
                    if let Some(text) = user_text {
                        self.app.input = Self::new_input();
                        for c in text.chars() {
                            self.app.input.input(ratatui_textarea::Input {
                                key: if c == '\n' {
                                    ratatui_textarea::Key::Enter
                                } else {
                                    ratatui_textarea::Key::Char(c)
                                },
                                ..Default::default()
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    // =========================================================================
    // D4: Keyboard actions on selected transcript item(s)
    // =========================================================================

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
    fn get_transcript_item_text(item: &TranscriptItem) -> Option<String> {
        match item {
            TranscriptItem::UserText { text, .. } => Some(text.clone()),
            TranscriptItem::AssistantText { text, .. } => Some(text.clone()),
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
            _ => None,
        }
    }

    /// Handle 'e' key: open edit command for selected item(s).
    async fn handle_transcript_edit(&mut self) -> Result<()> {
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
        Ok(())
    }

    /// Handle 'd' or Delete key: open delete confirmation modal.
    fn handle_transcript_delete(&mut self) {
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
    }

    /// Handle 'c' key: copy item text to clipboard.
    fn handle_transcript_copy(&mut self) {
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
    fn handle_transcript_rewind(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::tool_completed_to_transcript_items;
    use crate::types::TranscriptItem;
    use serde_json::json;

    #[test]
    fn tool_completed_preserves_fenced_diff_in_transcript() {
        let output = json!({
            "content": [
                {
                    "type": "text",
                    "text": "Applied patch successfully"
                },
                {
                    "type": "text",
                    "text": "```diff\n-old line\n+new line\n```"
                }
            ],
            "isError": false
        });

        let items = tool_completed_to_transcript_items(&output, None);

        assert_eq!(items.len(), 1);
        match &items[0] {
            TranscriptItem::ToolResultMarkdown(text) => {
                assert!(text.contains("Applied patch successfully"));
                assert!(text.contains("```diff"));
                assert!(text.contains("-old line"));
                assert!(text.contains("+new line"));
            }
            other => panic!("unexpected transcript item: {other:?}"),
        }
    }
}
