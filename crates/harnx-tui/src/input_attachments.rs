//! Manage input attachments, paste-to-file conversion and submitted previews.

use crate::types::{TranscriptItem, Tui};
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
    let opts = harnx_core::safety::TruncateOpts {
        offset: None,
        limit: None,
        head_lines: ATTACHMENT_PREVIEW_HEAD_LINES,
        tail_lines: ATTACHMENT_PREVIEW_TAIL_LINES,
        // Allow long single lines to be cropped in the middle too.
        line_head_bytes: ATTACHMENT_PREVIEW_MAX_BYTES / 2,
        line_tail_bytes: ATTACHMENT_PREVIEW_MAX_BYTES / 2,
        max_output_bytes: ATTACHMENT_PREVIEW_MAX_BYTES,
        marker: Some("...".to_string()),
    };

    let preview = harnx_core::safety::truncate_output(text, &opts);
    let preview = preview.trim_end_matches('\n').to_string();
    if preview.is_empty() {
        return None;
    }
    Some(preview)
}

impl Tui {
    fn can_accept_paste(&self) -> bool {
        let overlay_open = self.app.detail_view_open
            || self.app.transcript_browsing
            || !self.app.subagent_view_stack.is_empty();
        !overlay_open && (!self.has_root_cancellation() || self.cancellation_editor_restored())
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
    pub(super) async fn try_handle_attach_command(&mut self) -> bool {
        let Some(last_line) = self.app.input.lines().last() else {
            return false;
        };
        let command = last_line.trim().to_string();
        if let Some(path) = command.strip_prefix(".attach ") {
            self.attach_input_file(Path::new(path.trim())).await;
        } else if command == ".detach" {
            self.cleanup_attachments();
        } else if let Some(name) = command.strip_prefix(".detach ") {
            self.detach_named_attachment(name.trim());
        } else {
            return false;
        }
        // A recognized command is consumed even when its filesystem action fails.
        let lines = self.app.input.lines();
        let remaining = lines[..lines.len() - 1].join("\n");
        self.set_input_text(&remaining);
        true
    }

    async fn attach_input_file(&mut self, src: &Path) {
        if !src.exists() {
            self.app.transcript.push(TranscriptItem::ErrorText(format!(
                "File not found: {}",
                src.display()
            )));
            return;
        }
        let dir = match self.ensure_attachment_dir().await {
            Ok(dir) => dir,
            Err(error) => {
                self.app.transcript.push(TranscriptItem::ErrorText(format!(
                    "Failed to create attachment directory: {error}"
                )));
                return;
            }
        };
        let original_name = src
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| src.to_string_lossy().to_string());
        let display_name = unique_attachment_display_name(&self.app.attachments, &original_name);
        let dest = unique_attachment_storage_path(&dir, &original_name);
        match tokio::fs::copy(src, &dest).await {
            Ok(_) => self.app.attachments.push(crate::types::Attachment {
                path: dest,
                display_name,
            }),
            Err(error) => self.app.transcript.push(TranscriptItem::ErrorText(format!(
                "Failed to copy attachment: {error}"
            ))),
        }
    }

    fn detach_named_attachment(&mut self, name: &str) {
        for attachment in self
            .app
            .attachments
            .iter()
            .filter(|attachment| attachment.display_name == name)
        {
            if let Err(error) = std::fs::remove_file(&attachment.path) {
                self.app.transcript.push(TranscriptItem::ErrorText(format!(
                    "Failed to remove detached attachment file {}: {error}",
                    attachment.display_name
                )));
            }
        }
        self.app
            .attachments
            .retain(|attachment| attachment.display_name != name);
        if self.app.attachments.is_empty() {
            self.cleanup_attachments();
        }
    }

    pub(super) async fn handle_paste(&mut self, text: String) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(crate::types::ModalState::ConfirmToolUse(state)) = self.app.modal.as_mut() {
            state.last_key_at = std::time::Instant::now();
            state.message.insert_str(text);
            return;
        }
        if !self.can_accept_paste() {
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

    pub(super) async fn render_submitted_attachments(
        &mut self,
        attachments: &[crate::types::Attachment],
    ) {
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
}

#[cfg(test)]
mod tests {
    use super::paste_should_attach;

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
}
