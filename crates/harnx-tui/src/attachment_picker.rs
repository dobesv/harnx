//! Run a user-configured interactive picker with the TUI suspended.

use crate::types::{TranscriptItem, Tui};
use std::process::{Command, Stdio};

// qual:allow(coupling, oi) reason: "Tui handlers live in focused modules; the shared frontend state is defined in types.rs"
impl Tui {
    pub(super) async fn pick_attachment_files(&mut self) {
        if (self.has_root_cancellation() && !self.cancellation_editor_restored())
            || self.interaction_pending()
        {
            return;
        }
        let picker =
            std::env::var("HARNX_ATTACHMENT_PICKER").unwrap_or_else(|_| "fzf --multi".into());
        // Release the config lock before running an interactive process.
        if let Some(before) = self.config.write().tui_before_editor.as_mut() {
            before();
        }
        let result = tokio::task::spawn_blocking(move || run_picker(&picker)).await;
        if let Some(after) = self.config.write().tui_after_editor.as_mut() {
            after();
        }
        match result {
            Ok(Ok(paths)) if !paths.is_empty() => {
                let original_count = self.app.attachments.len();
                for path in paths {
                    self.attach_input_file(&path).await;
                }
                if self.app.attachments.len() > original_count {
                    self.app.pending_message = None;
                    self.clear_shared_pending_message().await;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => self
                .app
                .transcript
                .push(TranscriptItem::ErrorText(error.to_string())),
            Err(error) => self.app.transcript.push(TranscriptItem::ErrorText(format!(
                "File picker failed: {error}"
            ))),
        }
    }
}

fn run_picker(picker: &str) -> anyhow::Result<Vec<std::path::PathBuf>> {
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("sh");
        command.args(["-c", picker]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", picker]);
        command
    };
    let output = command
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()?;
    match output.status.code() {
        Some(0) => picker_paths(output.stdout),
        Some(1 | 130) => Ok(Vec::new()), // fzf: no match or cancelled
        _ => anyhow::bail!(
            "Attachment picker failed ({}). Install fzf or set HARNX_ATTACHMENT_PICKER.",
            output.status
        ),
    }
}

#[cfg(unix)]
fn picker_paths(output: Vec<u8>) -> anyhow::Result<Vec<std::path::PathBuf>> {
    use std::os::unix::ffi::OsStringExt;
    Ok(output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| std::ffi::OsString::from_vec(line.to_vec()).into())
        .collect())
}

#[cfg(windows)]
fn picker_paths(output: Vec<u8>) -> anyhow::Result<Vec<std::path::PathBuf>> {
    Ok(String::from_utf8(output)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(Into::into)
        .collect())
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::run_picker;

    #[test]
    fn picker_preserves_spaces_and_multiple_paths() {
        assert_eq!(
            run_picker("printf '%s\\n' 'a file.txt' 'second.png'").unwrap(),
            vec![
                std::path::PathBuf::from("a file.txt"),
                std::path::PathBuf::from("second.png")
            ]
        );
        use std::os::unix::ffi::OsStrExt;
        let paths = run_picker("printf 'file\\377.txt\\n'").unwrap();
        assert_eq!(paths[0].as_os_str().as_bytes(), b"file\xff.txt");
    }

    #[test]
    fn picker_cancellation_and_failure_are_distinct() {
        assert!(run_picker("exit 130").unwrap().is_empty());
        assert!(run_picker("exit 1").unwrap().is_empty());
        assert!(run_picker("exit 127")
            .unwrap_err()
            .to_string()
            .contains("Install fzf"));
    }
}
