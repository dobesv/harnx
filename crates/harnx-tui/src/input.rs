use crate::lifecycle::session_history_transcript_items;
use crate::render_helpers::{render_status_line, render_usage_line};
use crate::strip_ansi;
use crate::types::Tui;
use crate::types::{TranscriptItem, TuiEvent};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use crossterm::ExecutableCommand;
use harnx_core::event::{AgentEvent, AgentSource};
use harnx_render::pretty_error_string;
use harnx_runtime::config::{
    build_picker_context, list_assistant_agents, sort_sessions_for_picker,
};
use harnx_runtime::utils::pretty_yaml_block;
use ratatui_textarea::{Input as TextInput, Key};
use std::path::Path;

/// Byte budget for an attachment preview. Applied via `truncate_output`'s
/// `max_output_bytes` (and split across per-line head/tail byte limits), so it
/// is measured in bytes rather than characters — multibyte text may crop a
/// little earlier than an ASCII-only equivalent.
const ATTACHMENT_PREVIEW_MAX_BYTES: usize = 800;
const ATTACHMENT_PREVIEW_MAX_LINES: usize = 12;
/// Lines kept from the start of a cropped attachment preview.
const ATTACHMENT_PREVIEW_HEAD_LINES: usize = ATTACHMENT_PREVIEW_MAX_LINES / 2;
/// Lines kept from the end of a cropped attachment preview.
const ATTACHMENT_PREVIEW_TAIL_LINES: usize =
    ATTACHMENT_PREVIEW_MAX_LINES - ATTACHMENT_PREVIEW_HEAD_LINES;

/// A multi-line paste is only converted into an attachment when it is "large".
/// Small pastes (a handful of short lines) are inserted inline instead, so
/// pasting a couple of lines is not needlessly turned into a file.
///
/// A paste becomes an attachment when it exceeds EITHER of these limits.
const PASTE_ATTACHMENT_MAX_LINES: usize = 8;
const PASTE_ATTACHMENT_MAX_CHARS: usize = 512;

/// Returns true when a normalized (LF-only) paste is large enough to warrant
/// being stored as an attachment rather than inserted inline.
///
/// Line counting uses `str::lines()` so a single trailing newline does not
/// inflate the count (an 8-line paste ending in `\n` still counts as 8 lines).
fn paste_should_attach(text: &str) -> bool {
    let line_count = text.lines().count();
    line_count > PASTE_ATTACHMENT_MAX_LINES || text.chars().count() > PASTE_ATTACHMENT_MAX_CHARS
}

/// How long `start_prompt` waits for a prior prompt task to finish
/// cooperatively (after signalling its abort) before force-cancelling it
/// via `JoinHandle::abort`. Long enough for `bash_wait` and similar
/// cooperative tools to observe the abort and return; short enough that
/// the user does not feel a stall.
const PROMPT_TASK_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

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

pub(crate) async fn render_attachment_preview(path: &Path) -> Option<String> {
    let bytes = tokio::fs::read(path).await.ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    if text.is_empty() {
        return None;
    }

    // Reuse the same middle-cropping utility the agent uses so previews keep
    // the first and last lines instead of cutting off only the head. See
    // issue #770.
    let opts = harnx_mcp::safety::TruncateOpts {
        head_lines: ATTACHMENT_PREVIEW_HEAD_LINES,
        tail_lines: ATTACHMENT_PREVIEW_TAIL_LINES,
        // Allow long single lines to be cropped in the middle too.
        line_head_bytes: ATTACHMENT_PREVIEW_MAX_BYTES / 2,
        line_tail_bytes: ATTACHMENT_PREVIEW_MAX_BYTES / 2,
        max_output_bytes: ATTACHMENT_PREVIEW_MAX_BYTES,
        marker: Some("...".to_string()),
    };

    let preview = harnx_mcp::safety::truncate_output(text, &opts);
    let preview = preview.trim_end_matches('\n').to_string();
    if preview.is_empty() {
        return None;
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
    vec![TranscriptItem::ToolResultMarkdown {
        text: clean,
        rendered_cache: None,
    }]
}

impl Tui {
    fn open_detail_view_for_focused_item(&mut self) {
        self.app.detail_view_scroll = {
            let mut s = ratatui_widget_scrolling::ScrollState::new();
            s.follow = false;
            s
        };
        let focused_item = self
            .app
            .transcript_focus
            .and_then(|focus| self.app.transcript.get(focus));
        match focused_item {
            Some(TranscriptItem::CompactionMarker { detail_text, .. }) => {
                // The detail view always renders detail_text for a compaction
                // marker (see render_detail_view), so a raw-YAML lookup here
                // would be computed but never displayed. Skip it.
                self.app.detail_view_text = Some(detail_text.clone());
                self.app.detail_view_title = Some("Compacted session".to_string());
                self.app.detail_view_raw_yaml = None;
            }
            _ => {
                self.app.detail_view_text = None;
                self.app.detail_view_title = None;
                self.app.detail_view_raw_yaml = self
                    .selected_seq_range()
                    .and_then(|(from, to)| self.config.read().get_message_range_yaml(from, to));
            }
        }
        self.app.detail_view_open = true;
    }

    async fn handle_detail_view_key(&mut self, key: KeyEvent) -> Result<()> {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => {
                self.app.detail_view_open = false;
                // Return to browsing view (transcript_browsing stays true)
            }
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.app.detail_view_scroll.scroll_up();
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.app.detail_view_scroll.scroll_down();
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                for _ in 0..10 {
                    self.app.detail_view_scroll.scroll_up();
                }
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                for _ in 0..10 {
                    self.app.detail_view_scroll.scroll_down();
                }
            }
            (KeyCode::Char('e'), KeyModifiers::NONE) => {
                // Edit: close detail, run edit, then reopen detail with updated content.
                // Save focus and browsing state before editing since handle_transcript_edit
                // clears both.
                let had_focus = self.app.transcript_focus;
                let prior_browsing = self.app.transcript_browsing;
                self.app.detail_view_open = false;
                self.handle_transcript_edit().await?;
                // After edit, if we had a valid focused item, try to reopen detail view.
                // Note: transcript may have changed, so restore focus tentatively.
                if let Some(focus_idx) = had_focus {
                    // Check that the focus index still exists in the (possibly reloaded) transcript
                    if focus_idx < self.app.transcript.len() {
                        self.app.transcript_focus = Some(focus_idx);
                        self.app.transcript_selection_anchor = None;
                        self.app.transcript_browsing = prior_browsing;
                        self.open_detail_view_for_focused_item();
                    }
                }
            }
            (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('d'), KeyModifiers::NONE) => {
                // Delete: show confirm modal ON TOP of detail view (do NOT close detail_view_open)
                self.handle_transcript_delete();
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                // Rewind: show confirm modal ON TOP of detail view
                self.handle_transcript_rewind();
            }
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                self.handle_transcript_copy();
                // Show clipboard notice for 2 seconds
                self.app.copy_notice_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
            }
            _ => {} // all other keys silently consumed
        }
        Ok(())
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
                // Open detail view for current focused item
                self.open_detail_view_for_focused_item();
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
            _ => {} // consume all other keys to prevent bleed to input
        }
        Ok(())
    }

    pub(super) async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // If a modal is open, intercept all keys and route to modal handler
        if self.app.modal.is_some() {
            return self.handle_modal_key(key).await;
        }

        // While the detail view is open, handle navigation + mutation keys.
        // All other keys are silently consumed so they cannot bleed into the
        // hidden background input field or trigger background actions.
        if self.app.detail_view_open {
            return self.handle_detail_view_key(key).await;
        }

        // Browsing mode guard: when user is navigating history fullscreen
        if self.app.transcript_browsing {
            return self.handle_browsing_key(key).await;
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

                if let Some((ref session_id, ref cluster)) = self.active_remote_session {
                    let config = self.config.clone();
                    let session_id = session_id.clone();
                    let cluster = cluster.clone();
                    tokio::spawn(async move {
                        let server = { config.read().nats_server(&cluster).cloned() };
                        if let Ok(server) = server {
                            match harnx_runtime::config::Config::connect_nats_server(&server).await
                            {
                                Ok(client) => {
                                    if let Err(e) = harnx_runtime::send_control_command(
                                        &client,
                                        &session_id,
                                        harnx_runtime::ControlCommand::Cancel,
                                    )
                                    .await
                                    {
                                        log::warn!("Failed to send remote cancel command: {e}");
                                    }
                                }
                                Err(e) => {
                                    log::warn!("Failed to connect to NATS for remote cancel: {e}");
                                }
                            }
                        }
                    });
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
                    self.active_remote_session = None;
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
                } else if !self.app.completions.is_empty() {
                    self.app.completions.clear();
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
                self.open_detail_view_for_focused_item();
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
                        self.render_submitted_attachments(&attachments_snapshot)
                            .await;
                        self.pin_transcript_to_bottom();
                        self.app.input = Self::new_input();
                        self.run_command(&text).await?;
                        self.refresh_input_chrome();
                    } else {
                        // Guard: agent and session must both be active before
                        // submitting a prompt. If not, open the appropriate picker
                        // and keep the text in the input so the user can retry.
                        // The in-memory check (agent/session None) is always safe;
                        // resolve_initial_modal is only called when the check fires.
                        {
                            let needs_picker = {
                                let cfg = self.config.read();
                                cfg.agent.is_none() || cfg.session.is_none()
                            };
                            if needs_picker {
                                if let Some(modal) =
                                    crate::types::Tui::resolve_initial_modal(&self.config).await
                                {
                                    self.app.modal = Some(modal);
                                    return Ok(());
                                }
                            }
                        }
                        let attachments_snapshot = self.app.attachments.clone();
                        self.app.transcript.push(TranscriptItem::UserText {
                            text: text.clone(),
                            seq: None,
                            timestamp: Some(chrono::Utc::now()),
                        });
                        self.render_submitted_attachments(&attachments_snapshot)
                            .await;
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
                // While a transcript item is focused all unhandled keys are
                // silently consumed — they must not leak into the input widget.
                // (The specific action keys e/d/i/c/r are handled above; anything
                // else is irrelevant when focus is on a history item.)
                if self.app.transcript_focus.is_some() {
                    return Ok(());
                }
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
        // Ignore paste while the detail view or browsing view is open — same isolation
        // policy as handle_key: these overlays hide the input field.
        if self.app.detail_view_open || self.app.transcript_browsing {
            return;
        }
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
        if paste_should_attach(&text) {
            // Large paste: write to temp file and attach
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
            // Small paste (single line or a few short lines): insert inline
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
                if self.app.detail_view_open {
                    for _ in 0..3 {
                        self.app.detail_view_scroll.scroll_up();
                    }
                    return;
                }
                if self.app.transcript_browsing {
                    for _ in 0..3 {
                        self.app.browsing_view_scroll.scroll_up();
                    }
                    return;
                }
                for _ in 0..3 {
                    self.app.scroll_state.scroll_up();
                }
            }
            MouseEventKind::ScrollDown => {
                if self.app.detail_view_open {
                    for _ in 0..3 {
                        self.app.detail_view_scroll.scroll_down();
                    }
                    return;
                }
                if self.app.transcript_browsing {
                    for _ in 0..3 {
                        self.app.browsing_view_scroll.scroll_down();
                    }
                    return;
                }
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
                self.app.streaming_open = false;
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
                self.render_submitted_attachments(&pending.attachments)
                    .await;
                self.pin_transcript_to_bottom();
                self.refresh_input_chrome();
            }
            TuiEvent::ConfirmToolUse {
                tool_name,
                input_preview,
                reason,
                reply,
            } => {
                // A blocked tool-eval thread is waiting on `reply`. Show the
                // native modal and remember the channel; answering the modal
                // (handle_modal_key) sends the decision back.
                self.app.pending_confirm_reply = Some(reply);
                self.app.modal = Some(crate::types::ModalState::ConfirmToolUse {
                    tool_name,
                    input_preview,
                    reason,
                });
            }
        }
        Ok(())
    }

    /// Resolve an in-flight tool-use confirmation: send the decision to the
    /// blocked tool-eval thread and dismiss the modal.
    fn resolve_tool_confirm(&mut self, allow: bool) {
        if let Some(reply) = self.app.pending_confirm_reply.take() {
            let _ = reply.send(allow);
        }
        self.app.modal = None;
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
        self.render_submitted_attachments(&pending.attachments)
            .await;
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

    async fn render_submitted_attachments(&mut self, attachments: &[crate::types::Attachment]) {
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

            if let Some(preview) = render_attachment_preview(&attachment.path).await {
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
        use harnx_core::event::{
            ModelEvent, NoticeEvent, SessionEvent, ToolEvent, TurnEvent, UserEvent,
        };

        let is_thought = matches!(&event, AgentEvent::Model(ModelEvent::ThoughtChunk { .. }));
        let is_usage = matches!(&event, AgentEvent::Model(ModelEvent::Usage { .. }));
        // No streaming-run bookkeeping is needed here: any event that renders a
        // visible transcript item (tool call, tool result, notice, plan, …)
        // becomes the trailing item, which ends the open streaming run on its
        // own — the next MessageChunk sees a non-AssistantText tail and starts
        // a fresh block. See `append_streaming_assistant_chunk`.
        //
        // Handle LogSeqAssigned before any heading/thought side-effects — it
        // is a pure seq-assignment event and should not create stray headings
        // or flush pending thoughts.
        if let AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }) = event {
            // Try to backfill the seq into the most recent unsequenced transcript
            // item (UserText, AssistantText, or ToolCall). If an item is found and
            // patched, the seq has been consumed — clear pending_tool_seq.  If no
            // item is found yet (e.g. ToolEvent::Started arrives after this event),
            // store seq in pending_tool_seq so the upcoming ToolCall can pick it up.
            let mut backfilled = false;
            for item in self.app.transcript.iter_mut().rev() {
                match item {
                    // Only backfill "live" entries — items with a timestamp are
                    // created during an active session.  The agent banner is
                    // AssistantText { seq: None, timestamp: None } and must not
                    // consume a seq that belongs to the first real message.
                    TranscriptItem::UserText {
                        seq: item_seq @ None,
                        timestamp: Some(_),
                        ..
                    }
                    | TranscriptItem::AssistantText {
                        seq: item_seq @ None,
                        timestamp: Some(_),
                        ..
                    }
                    | TranscriptItem::ToolCall {
                        seq: item_seq @ None,
                        timestamp: Some(_),
                        ..
                    } => {
                        *item_seq = Some(seq);
                        backfilled = true;
                        break;
                    }
                    _ => {}
                }
            }
            if backfilled {
                // Seq consumed by an existing item; clear any pending slot.
                self.app.pending_tool_seq = None;
            } else {
                // No existing item to patch; save for the next ToolCall creation.
                self.app.pending_tool_seq = Some(seq);
            }
            return;
        }

        if let AgentEvent::Turn(TurnEvent::ModelFallback { ref to, .. }) = event {
            let new_source = self.app.last_ui_output_source.clone().map(|mut s| {
                s.model = Some(to.clone());
                s
            });
            self.render_ui_output_heading(new_source.as_ref(), false);
            return;
        }
        if let AgentEvent::Session(SessionEvent::ModelChanged { ref to, .. }) = event {
            let new_source = self.app.last_ui_output_source.clone().map(|mut s| {
                s.model = Some(to.clone());
                s
            });
            self.render_ui_output_heading(new_source.as_ref(), false);
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
            AgentEvent::User(UserEvent::Message { content }) => {
                // Replayed/attached history item — NOT a live turn. Use
                // `timestamp: None` (matching the agent banner) so this row is
                // excluded from the `LogSeqAssigned` backfill heuristic above,
                // which only patches "live" items (`timestamp: Some`). A fresh
                // timestamp would let the next live seq bind to this replayed
                // row, breaking edit/delete/rewind targeting.
                vec![TranscriptItem::UserText {
                    text: content,
                    seq: None,
                    timestamp: None,
                }]
            }
            AgentEvent::Tool(ToolEvent::Completed {
                output, markdown, ..
            }) => tool_completed_to_transcript_items(&output, markdown.as_deref()),
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                let text = concat_text_blocks(&blocks);
                if text.is_empty() {
                    vec![]
                } else {
                    self.app.streamed_text_this_turn = true;
                    self.append_streaming_assistant_chunk(&text);
                    self.pin_transcript_to_bottom();
                    vec![]
                }
            }
            AgentEvent::Model(ModelEvent::Final { output, usage }) => {
                self.flush_pending_thought();
                self.app.llm_busy = false;
                self.active_remote_session = None;
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
                    if self.app.streamed_text_this_turn {
                        // We streamed this turn, so the streamed run is the
                        // trailing block of AssistantText items. Replace it
                        // with the model's canonical final `output` (collapsing
                        // any trailing run to a single block) so streamed text
                        // is never duplicated by the final message.
                        let tail_start = self
                            .app
                            .transcript
                            .iter()
                            .rposition(|item| !matches!(item, TranscriptItem::AssistantText { .. }))
                            .map_or(0, |idx| idx + 1);

                        if tail_start < self.app.transcript.len() {
                            let mut tail = self.app.transcript.split_off(tail_start);
                            if let Some(TranscriptItem::AssistantText {
                                text,
                                rendered_cache,
                                ..
                            }) = tail.first_mut()
                            {
                                *text = output;
                                *rendered_cache = None;
                            }
                            tail.truncate(1);
                            self.app.transcript.extend(tail);
                        } else {
                            self.app.transcript.push(TranscriptItem::AssistantText {
                                text: output,
                                seq: None,
                                timestamp: Some(chrono::Utc::now()),
                                rendered_cache: None,
                            });
                        }
                    } else {
                        // Nothing streamed this turn (non-streaming client, or
                        // an empty stream): append the final text as a fresh
                        // block.
                        self.app.transcript.push(TranscriptItem::AssistantText {
                            text: output,
                            seq: None,
                            timestamp: Some(chrono::Utc::now()),
                            rendered_cache: None,
                        });
                    }
                    self.pin_transcript_to_bottom();
                }
                self.app.streaming_open = false;
                self.app.streamed_text_this_turn = false;
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
                self.active_remote_session = None;
                // Mirrors the Final cleanup: drop the per-task abort
                // signal (task has exited) and clear the shared pending
                // channel so its content can't leak into the next task.
                self.current_prompt_abort = None;
                *self.shared_pending_message.lock().await = None;
                self.app.streaming_open = false;
                // Symmetric with the Final handler: reset the per-turn
                // streamed-text flag so a streamed chunk before an error
                // can't suppress a later turn's final text.
                self.app.streamed_text_this_turn = false;
                self.app.last_ui_output_source = None;
                self.app.transcript.push(TranscriptItem::ErrorText(err));

                // Do NOT auto-replay the pending message on error. Replaying the
                // exact input that just failed would loop forever for persistent
                // failures (invalid input, repeated network errors). Instead,
                // restore it as an editable draft so the user can review/edit and
                // resubmit it with Enter, breaking the retry loop (issue #199).
                if let Some(pending) = self.app.pending_message.take() {
                    self.set_input_text(&pending.text);
                    self.app.attachments = pending.attachments;
                    self.app.attachment_dir = pending.attachment_dir;
                    self.app.paste_count = pending.paste_count;
                    self.app.transcript.push(TranscriptItem::SystemText(
                        "Queued message not sent due to error. Press Enter to retry.".to_string(),
                    ));
                }
                self.pin_transcript_to_bottom();
                self.refresh_input_chrome();
                vec![]
            }
            AgentEvent::Model(ModelEvent::ThoughtChunk { blocks }) => {
                let text = concat_text_blocks(&blocks);
                let clean = strip_ansi(&text)
                    .trim_start_matches("<think>")
                    .trim_end_matches("</think>")
                    .to_string();
                // Only skip genuinely empty chunks (e.g. a chunk that was just a
                // `<think>` tag). Whitespace-only chunks (a lone "\n" between
                // streamed thought lines) must be preserved so the accumulated
                // thought keeps its line breaks — same class of bug as #862 in
                // the ACP client's message/thought chunk handling.
                if clean.is_empty() {
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
                    seq: self.app.pending_tool_seq,
                    timestamp: Some(chrono::Utc::now()),
                    rendered_cache: None,
                }]
            }
            AgentEvent::Tool(ToolEvent::Blocked {
                name,
                input,
                reason,
                ..
            }) => {
                let body = {
                    let reason_text = format!("⊘ blocked: {reason}");
                    let input_text = match &input {
                        serde_json::Value::Null => String::new(),
                        _ => {
                            let yaml = harnx_runtime::utils::pretty_yaml_block(&input);
                            format!("{yaml}\n")
                        }
                    };
                    let full = format!("{input_text}{reason_text}");
                    Some(crate::types::ToolCallBody::Markdown(full))
                };
                vec![TranscriptItem::ToolCall {
                    tool_name: name,
                    body,
                    seq: self.app.pending_tool_seq,
                    timestamp: Some(chrono::Utc::now()),
                    rendered_cache: None,
                }]
            }
            AgentEvent::Session(SessionEvent::CompactingStarted) => {
                vec![TranscriptItem::SystemText(
                    "Compacting session…".to_string(),
                )]
            }
            AgentEvent::Session(SessionEvent::CompactingCompleted) => {
                self.app.transcript = session_history_transcript_items(&self.config);
                self.app.streaming_open = false;
                // A compaction can land mid-turn after some assistant text has
                // already streamed. The rebuild drops that streamed row, so the
                // per-turn flag must also reset — otherwise the next
                // ModelEvent::Final sees streamed_text_this_turn == true and
                // skips rendering the final assistant row.
                self.app.streamed_text_this_turn = false;
                // The rebuild drops all SourceHeading entries, so the next
                // output must re-emit its heading even if it shares the prior
                // source. Without this, the first post-compaction message would
                // render without an agent label.
                self.app.last_ui_output_source = None;
                self.clear_usage_tracking();
                // The transcript is entirely rebuilt, so any prior focus/anchor
                // indices reference now-different items even when still in
                // bounds. Clear selection/detail state unconditionally.
                self.app.transcript_focus = None;
                self.app.transcript_selection_anchor = None;
                self.pin_transcript_to_bottom();
                vec![]
            }
            AgentEvent::Session(SessionEvent::CompactingFailed(err)) => {
                vec![TranscriptItem::ErrorText(format!(
                    "Compaction failed: {err}"
                ))]
            }
            AgentEvent::Session(SessionEvent::TitleGenerationFailed(err)) => {
                vec![TranscriptItem::ErrorText(format!(
                    "Title generation failed: {err}"
                ))]
            }
            AgentEvent::Session(SessionEvent::TitleUpdated(title)) => {
                let _ = std::io::stdout().execute(crossterm::terminal::SetTitle(&title));
                vec![]
            }
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
            self.app.streaming_open = false;
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
        self.app.streaming_open = false;

        self.app.streamed_text_this_turn = false;

        let remote_info = {
            let guard = self.config.read();
            let agent_and_cluster = guard.remote_agent.clone();
            let session_id = guard
                .session
                .as_ref()
                .map(|s| s.id().to_string())
                .unwrap_or_default();
            agent_and_cluster.map(|(_, cluster)| (session_id, cluster))
        };
        self.active_remote_session = remote_info;

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
            // Check if we're running a remote agent
            // Clone remote_agent before spawning to avoid holding lock across await
            let remote_agent = {
                let guard = ctx.config.read();
                guard.remote_agent.clone()
            };
            // Guard is dropped here before any await

            let result: Result<()> = if let Some((agent, cluster)) = remote_agent {
                // Run remote agent via NATS thin-client
                Self::run_remote_prompt_task(msg, ctx, agent, cluster).await
            } else {
                // Run local agent
                Self::run_prompt_task(msg, ctx).await
            };

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

            if matches!(cmd, ".agent" | ".session") && args.iter().all(|arg| arg.is_empty()) {
                return vec![];
            }

            // Fetch agents async outside the config lock to avoid holding a
            // parking_lot read guard across an await point.
            let precomputed_agents = if cmd == ".agent" && args.len() == 1 {
                list_assistant_agents().await
            } else {
                Vec::new()
            };

            // When completing .session in remote-agent context, use the async
            // helper that queries the NATS KV index with a short timeout.
            // Otherwise, fall back to local sessions.
            if cmd == ".session" && args.len() == 1 {
                let cluster = self
                    .config
                    .read()
                    .remote_agent
                    .as_ref()
                    .map(|(_, c)| c.clone());
                let cfg = self.config.read().clone();
                let sessions = cfg.list_sessions_for_completion(cluster.as_deref()).await;
                return sessions.into_iter().map(|s| (s, None)).collect();
            }

            return self
                .config
                .read()
                .command_complete(cmd, &args, precomputed_agents);
        }

        vec![]
    }

    async fn open_agent_picker(&mut self) {
        self.app.modal = Some(crate::types::ModalState::AgentPicker {
            agents: list_assistant_agents().await,
            selected: 0,
            query: String::new(),
        });
    }

    pub(crate) async fn open_session_picker(&mut self) {
        let (is_remote, cluster) = {
            let cfg = self.config.read();
            if let Some((_, ref cluster)) = cfg.remote_agent {
                (true, cluster.clone())
            } else {
                (false, String::new())
            }
        };

        let (sessions, fetch_error) = if is_remote {
            let cfg = self.config.read().clone();
            match cfg.list_remote_sessions_with_meta(&cluster).await {
                Ok(s) => (s, None),
                Err(e) => {
                    log::warn!(
                        "Failed to list remote sessions for cluster '{}': {:#}",
                        cluster,
                        e
                    );
                    (vec![], Some(format!("remote sessions unavailable: {e:#}")))
                }
            }
        } else {
            (self.config.read().list_sessions_with_meta(), None)
        };

        let ctx = build_picker_context(None);
        let sessions = sort_sessions_for_picker(sessions, &ctx);
        let origin_agent = self
            .config
            .read()
            .agent
            .as_ref()
            .map(|a| a.name().to_string());
        let origin_session = self
            .config
            .read()
            .session
            .as_ref()
            .map(|s| s.id().to_string());
        self.app.modal = Some(crate::types::ModalState::SessionPicker {
            sessions,
            selected: 0,
            origin_agent,
            origin_session,
            error: fetch_error,
        });
    }

    async fn maybe_open_picker_after_command(
        &mut self,
        outcome: harnx_runtime::commands::CommandOutcome,
        prev_agent: Option<String>,
    ) {
        match outcome {
            harnx_runtime::commands::CommandOutcome::Continue => {
                let (curr_agent, session_missing) = {
                    let cfg = self.config.read();
                    (
                        cfg.agent.as_ref().map(|a| a.name().to_string()),
                        cfg.session.is_none(),
                    )
                };
                if prev_agent != curr_agent && session_missing {
                    self.open_session_picker().await;
                }
            }
            harnx_runtime::commands::CommandOutcome::Exit => {
                self.app.should_quit = true;
            }
            harnx_runtime::commands::CommandOutcome::OpenAgentPicker => {
                self.open_agent_picker().await;
            }
            harnx_runtime::commands::CommandOutcome::OpenSessionPicker => {
                self.open_session_picker().await;
            }
        }
    }

    fn reconcile_transcript_after_command(
        &mut self,
        prev_session: Option<String>,
        prev_agent: Option<String>,
        command_was: &str,
    ) {
        let (curr_session, curr_agent) = {
            let cfg = self.config.read();
            let s = cfg.session.as_ref().map(|s| s.id().to_string());
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
        self.app.streaming_open = false;
        self.app.streamed_text_this_turn = false;
        // Reset scroll state so the widget doesn't subtract-overflow when
        // the rebuilt transcript is shorter than the previous one.
        self.app.scroll_state = ratatui_widget_scrolling::ScrollState::new();
        self.app.transcript = Self::build_initial_transcript(&self.config);
        self.pin_transcript_to_bottom();
    }

    fn try_handle_info_overlay(&mut self, line_cmd: &str) -> bool {
        let is_info_agent =
            line_cmd.starts_with(".info agent") || line_cmd.starts_with("/info agent");
        let is_info_session =
            line_cmd.starts_with(".info session") || line_cmd.starts_with("/info session");

        if !is_info_agent && !is_info_session {
            return false;
        }

        let Ok(tokens) = shell_words::split(line_cmd) else {
            self.app.transcript.push(TranscriptItem::ErrorText(
                "Unclosed quotes in command".to_string(),
            ));
            return true;
        };

        let result = if is_info_agent {
            self.resolve_info_agent_target(&tokens)
                .and_then(|agent_name| {
                    let cfg = self.config.read();
                    harnx_runtime::config::render_agent_dump(&cfg, &agent_name)
                })
        } else {
            self.resolve_info_session_target(&tokens)
                .and_then(|(agent_name, session_id)| {
                    harnx_runtime::config::render_session_dump(agent_name.as_deref(), &session_id)
                })
        };

        let display_text = result.unwrap_or_else(|err| format!("Error: {}", err));
        let title = if is_info_agent {
            "Agent Info"
        } else {
            "Session Info"
        };
        self.open_info_overlay(display_text, title);
        true
    }

    fn resolve_info_agent_target(&self, tokens: &[String]) -> anyhow::Result<String> {
        let agent_name = if tokens.len() > 2 {
            tokens[2].clone()
        } else {
            match self.config.read().agent.as_ref() {
                Some(a) => a.name().to_string(),
                None => String::new(),
            }
        };

        if agent_name.is_empty() {
            Err(anyhow::anyhow!(
                "No active agent and no agent name provided. Usage: .info agent [<name>]"
            ))
        } else {
            Ok(agent_name)
        }
    }

    fn resolve_info_session_target(
        &self,
        tokens: &[String],
    ) -> anyhow::Result<(Option<String>, String)> {
        let (agent_name, session_id) = if tokens.len() > 3 {
            (Some(tokens[2].clone()), tokens[3].clone())
        } else if tokens.len() == 3 {
            let cfg = self.config.read();
            let a = cfg.agent.as_ref().map(|x| x.name().to_string());
            (a, tokens[2].clone())
        } else {
            let cfg = self.config.read();
            let a = cfg.agent.as_ref().map(|x| x.name().to_string());
            let s = cfg.session.as_ref().map(|x| x.id().to_string());
            match (a, s) {
                (a_opt, Some(session)) => (a_opt, session),
                _ => (None, String::new()),
            }
        };

        if session_id.is_empty() {
            Err(anyhow::anyhow!(
                "No active session or insufficient arguments. Usage: .info session [<agent> <id>]"
            ))
        } else {
            Ok((agent_name, session_id))
        }
    }

    fn open_info_overlay(&mut self, text: String, title: &str) {
        self.app.detail_view_scroll = {
            let mut s = ratatui_widget_scrolling::ScrollState::new();
            s.follow = false;
            s
        };
        self.app.detail_view_text = Some(text);
        self.app.detail_view_title = Some(title.to_string());
        self.app.detail_view_raw_yaml = None;
        self.app.detail_view_open = true;
    }

    pub(super) async fn run_command(&mut self, line: &str) -> Result<()> {
        if self.try_handle_info_overlay(line.trim_start()) {
            return Ok(());
        }
        let prev_session = self
            .config
            .read()
            .session
            .as_ref()
            .map(|s| s.id().to_string());
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

        self.finish_command(
            result,
            clean,
            (line, prev_session, prev_agent, is_mutation_command),
        )
        .await;

        Ok(())
    }

    async fn finish_command(
        &mut self,
        result: Result<harnx_runtime::commands::CommandOutcome>,
        clean: String,
        ctx: (&str, Option<String>, Option<String>, bool),
    ) {
        let (line, prev_session, prev_agent, is_mutation_command) = ctx;
        match result {
            Ok(outcome) => {
                self.maybe_open_picker_after_command(outcome, prev_agent.clone())
                    .await;
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
            Some(crate::types::ModalState::ConfirmToolUse { .. }) => {
                match (key.code, key.modifiers) {
                    // Default is deny ([y/N]): only an explicit 'y' allows the call.
                    (KeyCode::Char('y') | KeyCode::Char('Y'), KeyModifiers::NONE) => {
                        self.resolve_tool_confirm(true);
                    }
                    // Deny on n/N/Esc/Enter, and on Ctrl+C so the blocked tool-eval
                    // thread is never left waiting.
                    (KeyCode::Char('n') | KeyCode::Char('N'), KeyModifiers::NONE)
                    | (KeyCode::Esc, KeyModifiers::NONE)
                    | (KeyCode::Enter, KeyModifiers::NONE)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        self.resolve_tool_confirm(false);
                    }
                    _ => {}
                }
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
        // Ctrl+D always exits — no prompt running in picker state.
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
            self.app.should_quit = true;
            return Ok(());
        }
        // Ctrl+C exits in picker state — there is no in-flight prompt to abort.
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.app.should_quit = true;
            return Ok(());
        }
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
                if let Some(crate::types::ModalState::AgentPicker {
                    selected,
                    agents,
                    query,
                }) = self.app.modal.as_mut()
                {
                    let filtered_len =
                        crate::types::ModalState::filtered_agents(agents, query).len();
                    if *selected + 1 < filtered_len {
                        *selected += 1;
                    }
                } else if let Some(crate::types::ModalState::SessionPicker {
                    selected,
                    sessions,
                    ..
                }) = self.app.modal.as_mut()
                {
                    // Total items = 1 ("New session") + sessions.len(), so max
                    // valid index is sessions.len().
                    if *selected < sessions.len() {
                        *selected += 1;
                    }
                }
            }
            KeyCode::Enter => {
                let modal = self.app.modal.take();
                match modal {
                    Some(crate::types::ModalState::AgentPicker {
                        agents,
                        selected,
                        query,
                    }) => {
                        // Resolve the selected index against the filtered list.
                        let filtered = crate::types::ModalState::filtered_agents(&agents, &query);
                        let filtered: Vec<&str> = filtered.iter().map(|a| a.as_str()).collect();
                        if selected >= filtered.len() {
                            // Nothing valid selected (e.g. filter yields empty list) —
                            // keep the picker open so the user can adjust the query.
                            self.app.modal = Some(crate::types::ModalState::AgentPicker {
                                agents,
                                selected,
                                query,
                            });
                        } else {
                            let agent_name = filtered[selected].to_string();

                            let prev_session = self
                                .config
                                .read()
                                .session
                                .as_ref()
                                .map(|s| s.id().to_string());
                            let prev_agent = self
                                .config
                                .read()
                                .agent
                                .as_ref()
                                .map(|a| a.name().to_string());

                            // Activate the agent immediately so sessions_dir() is
                            // already scoped to the correct per-agent directory.
                            if let Err(e) = self.config.write().use_agent_by_name(&agent_name) {
                                self.app.modal = Some(crate::types::ModalState::AgentPicker {
                                    agents,
                                    selected,
                                    query,
                                });
                                return Err(e);
                            }

                            let sessions = self.config.read().list_sessions_with_meta();
                            let ctx = build_picker_context(None);
                            let sessions = sort_sessions_for_picker(sessions, &ctx);
                            // Always show SessionPicker so the user can pick "New session"
                            // (index 0) or an existing session. Carry the pre-activation
                            // origin state so reconcile_transcript_after_command sees the
                            // full transition.
                            self.app.modal = Some(crate::types::ModalState::SessionPicker {
                                sessions,
                                selected: 0,
                                origin_agent: prev_agent,
                                origin_session: prev_session,
                                error: None,
                            });
                        }
                    }
                    Some(crate::types::ModalState::SessionPicker {
                        sessions,
                        selected,
                        origin_agent,
                        origin_session,
                        error: _error,
                    }) => {
                        // Index 0 = "New session"; index N (1‥) = sessions[N-1].
                        if selected == 0 {
                            // Create a new session.
                            if let Err(e) = self.config.write().use_session(None) {
                                self.app.modal = Some(crate::types::ModalState::SessionPicker {
                                    sessions,
                                    selected,
                                    origin_agent,
                                    origin_session,
                                    error: None,
                                });
                                return Err(e);
                            }
                            let llm_busy = self.app.llm_busy;
                            let pending = self.app.pending_message.is_some();
                            Self::refresh_input_chrome_from_state(
                                &self.config,
                                &mut self.app,
                                llm_busy,
                                pending,
                            );
                            self.reconcile_transcript_after_command(
                                origin_session,
                                origin_agent,
                                ".session",
                            );
                        } else if selected > sessions.len() {
                            // Index out of range — keep picker open.
                            self.app.modal = Some(crate::types::ModalState::SessionPicker {
                                sessions,
                                selected,
                                origin_agent,
                                origin_session,
                                error: None,
                            });
                        } else {
                            // Existing session at sessions[selected - 1].
                            let session_name = sessions[selected - 1].id.clone();

                            if let Err(e) = self.config.write().use_session(Some(&session_name)) {
                                self.app.modal = Some(crate::types::ModalState::SessionPicker {
                                    sessions,
                                    selected,
                                    origin_agent,
                                    origin_session,
                                    error: None,
                                });
                                return Err(e);
                            }

                            let llm_busy = self.app.llm_busy;
                            let pending = self.app.pending_message.is_some();
                            Self::refresh_input_chrome_from_state(
                                &self.config,
                                &mut self.app,
                                llm_busy,
                                pending,
                            );
                            // Use origin_* to reflect the full transition from the start
                            // of the picker flow (not just the session half of the switch).
                            self.reconcile_transcript_after_command(
                                origin_session,
                                origin_agent,
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
                // ESC behaviour depends on which picker is open and how it was reached.
                //
                // SessionPicker:
                //   • origin_session is Some → restore origin agent+session (mid-switch cancel)
                //   • origin_session is None AND origin_agent is None → came from AgentPicker
                //     during startup; go back to the AgentPicker
                //   • origin_session is None AND origin_agent is Some → direct startup with
                //     `-a <agent>` and no session; ESC exits the process (#467)
                //
                // AgentPicker:
                //   • agent already active (mid-switch) → dismiss picker (cancel switch)
                //   • no agent active (startup) → exit the process (#467)
                let mut should_restore_origin = false;
                let mut should_show_agent_picker = false;
                if let Some(crate::types::ModalState::SessionPicker {
                    origin_session,
                    origin_agent,
                    ..
                }) = self.app.modal.as_ref()
                {
                    if origin_session.is_some() {
                        should_restore_origin = true;
                    } else if origin_agent.is_none() {
                        // Came from AgentPicker (startup flow: harnx → pick agent → session
                        // picker). ESC goes back to the AgentPicker.
                        should_show_agent_picker = true;
                    } else {
                        // Direct startup with `-a <agent>`, no prior session. ESC exits.
                        self.app.should_quit = true;
                    }
                } else if matches!(
                    self.app.modal,
                    Some(crate::types::ModalState::AgentPicker { .. })
                ) {
                    if self.config.read().agent.is_some() {
                        // Agent already active — mid-switch cancel: just dismiss the picker.
                        self.app.modal = None;
                    } else {
                        // No agent active (startup). ESC exits the process (#467).
                        self.app.should_quit = true;
                    }
                } else if self.app.modal.is_some() {
                    self.app.modal = None;
                }

                if should_show_agent_picker {
                    // Replace the SessionPicker with a fresh AgentPicker so the user can
                    // pick a different agent (or the same one again).
                    let agents = harnx_runtime::config::list_assistant_agents().await;
                    self.app.modal = Some(crate::types::ModalState::AgentPicker {
                        agents,
                        selected: 0,
                        query: String::new(),
                    });
                } else if should_restore_origin {
                    if let Some(crate::types::ModalState::SessionPicker {
                        origin_agent,
                        origin_session,
                        ..
                    }) = self.app.modal.take()
                    {
                        if let Some(agent) = origin_agent {
                            let _ = self.config.write().use_agent_by_name(&agent);
                        }
                        if let Some(session) = origin_session {
                            let _ = self.config.write().use_session(Some(&session));
                        }

                        let llm_busy = self.app.llm_busy;
                        let pending = self.app.pending_message.is_some();
                        Self::refresh_input_chrome_from_state(
                            &self.config,
                            &mut self.app,
                            llm_busy,
                            pending,
                        );
                    }
                }
            }

            // Typing characters filters the AgentPicker list.
            KeyCode::Char(c)
                if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
            {
                if let Some(crate::types::ModalState::AgentPicker {
                    query, selected, ..
                }) = self.app.modal.as_mut()
                {
                    query.push(c);
                    *selected = 0; // reset to top of filtered list
                }
            }
            KeyCode::Backspace => {
                if let Some(crate::types::ModalState::AgentPicker {
                    query, selected, ..
                }) = self.app.modal.as_mut()
                {
                    query.pop();
                    *selected = 0;
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
                    self.app.detail_view_open = false;
                    self.app.detail_view_raw_yaml = None;
                    self.app.detail_view_text = None;
                    self.app.detail_view_title = None;
                    self.app.transcript_browsing = false;
                    self.app.transcript_focus = None;
                    self.app.transcript_selection_anchor = None;
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
                    self.app.detail_view_open = false;
                    self.app.detail_view_raw_yaml = None;
                    self.app.detail_view_text = None;
                    self.app.detail_view_title = None;
                    self.app.transcript_browsing = false;
                    self.app.transcript_focus = None;
                    self.app.transcript_selection_anchor = None;
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
        self.app.transcript_browsing = false;
        self.app.scroll_state.follow = true;
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
        self.app.transcript_browsing = false;
        self.app.scroll_state.follow = true;
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
    use super::paste_should_attach;
    use super::tool_completed_to_transcript_items;
    use crate::types::TranscriptItem;
    use serde_json::json;

    #[test]
    fn paste_should_attach_thresholds() {
        // Single line, short: inline.
        assert!(!paste_should_attach("just one line"));
        // A few short lines: inline.
        assert!(!paste_should_attach("line one\nline two\nline three"));
        // Exactly at the line limit (8 lines): still inline.
        assert!(!paste_should_attach("1\n2\n3\n4\n5\n6\n7\n8"));
        // 8 content lines with a trailing newline must still count as 8 lines
        // (str::lines ignores the trailing newline): inline.
        assert!(!paste_should_attach("1\n2\n3\n4\n5\n6\n7\n8\n"));
        // Over the line limit (9 lines): attach.
        assert!(paste_should_attach("1\n2\n3\n4\n5\n6\n7\n8\n9"));
        // Two lines but over the character limit: attach.
        let long = "a".repeat(600);
        assert!(paste_should_attach(&format!("{long}\n{long}")));
    }

    #[test]
    fn paste_should_attach_char_boundary() {
        // Exactly at the char limit (512): inline.
        let at_limit = "a".repeat(512);
        assert_eq!(at_limit.chars().count(), 512);
        assert!(!paste_should_attach(&at_limit));
        // One over the char limit (513): attach.
        let over_limit = "a".repeat(513);
        assert!(paste_should_attach(&over_limit));
    }

    #[test]
    fn paste_should_attach_counts_chars_not_bytes() {
        // 300 multibyte chars (each is multiple bytes) stays under the 512-char
        // limit even though its byte length far exceeds 512: inline.
        let multibyte = "é".repeat(300);
        assert_eq!(multibyte.chars().count(), 300);
        assert!(
            multibyte.len() > 512,
            "byte length should exceed char limit"
        );
        assert!(!paste_should_attach(&multibyte));
        // 513 multibyte chars exceeds the char limit: attach.
        assert!(paste_should_attach(&"é".repeat(513)));
    }

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
            TranscriptItem::ToolResultMarkdown { text, .. } => {
                assert!(text.contains("Applied patch successfully"));
                assert!(text.contains("```diff"));
                assert!(text.contains("-old line"));
                assert!(text.contains("+new line"));
            }
            other => panic!("unexpected transcript item: {other:?}"),
        }
    }
}
