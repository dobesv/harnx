//! `.credentials` — manage your own tool-server credentials from the TUI
//! (decision 11). Acts for the operator's identity, never the active
//! session's owner; secrets are only ever entered on the tool server's own
//! connect page.

use crate::types::{TranscriptItem, Tui, TuiEvent};
use anyhow::Result;
use harnx_runtime::config::{Config, ConfigLock};
use harnx_runtime::operator_tools::credentials::{
    parse_credentials_action, run_credentials_command, CredentialsAction, CredentialsProgress,
};
use harnx_runtime::utils::AbortSignal;
use std::sync::Arc;

/// Recognize a `.credentials` / `/credentials` dot-command line. Lines with
/// another prefix return `None`; a recognized prefix with bad arguments
/// returns the parse error for inline display.
pub(super) fn parse_credentials_dot_command(line: &str) -> Option<Result<CredentialsAction>> {
    let line = line.trim_start();
    let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
    if matches!(command, ".credentials" | "/credentials") {
        Some(parse_credentials_action(rest))
    } else {
        None
    }
}

/// Whose credentials `.credentials` manages: the same default the CLI
/// stamps on new sessions for the active cluster — not the active session's
/// owner, which may be another user.
fn identity_note(config: &Config) -> String {
    let cluster = config
        .remote_agent
        .as_ref()
        .map(|(_, cluster)| cluster.as_str())
        .unwrap_or_else(|| config.default_cluster_key());
    match config.default_user_id_for_cluster(cluster) {
        Some(user_id) => format!("Managing credentials for {user_id}."),
        None => "No user_id set; managing credentials for the ownerless profile (only --single-user tool servers accept it)."
            .to_string(),
    }
}

// qual:allow(coupling, oi) reason: "Credentials commands extend the shared Tui, following the per-concern module split (see interaction_confirmation.rs)."
impl Tui {
    /// Dispatch a parsed `.credentials` action on the background task, with
    /// the same fencing and busy-state handling as operator tool commands.
    pub(super) fn start_credentials_command(&mut self, action: CredentialsAction) {
        self.retire_prompt_task();
        let abort = harnx_runtime::utils::create_abort_signal();
        self.current_prompt_abort = Some(abort.clone());
        // Not an inference turn: Ctrl+C cancels the connect flow, not the log.
        self.active_remote_session = None;
        self.app.llm_busy = true;
        crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Working);
        let note = identity_note(&self.config.read().clone());
        self.app.transcript.push(TranscriptItem::SystemText(note));
        self.pin_transcript_to_bottom();
        self.refresh_input_chrome();
        let snapshot = Arc::new(ConfigLock::new(self.config.read().clone()));
        let worker = self.local_worker.clone();
        let event_tx = self.event_tx.clone();
        self.current_prompt_handle = Some(tokio::spawn(async move {
            let task = abort.clone();
            let mut report = |progress: CredentialsProgress| {
                let CredentialsProgress::ConnectUrl { url, message } = progress;
                // A markdown link row gives the URL as a keyboard-accessible
                // OSC 8 hyperlink; the raw URL stays visible as text.
                let text = if message.is_empty() {
                    format!("[Open the connect page]({url})\n{url}")
                } else {
                    format!("{message}\n[Open the connect page]({url})\n{url}")
                };
                let _ = event_tx.send(TuiEvent::CredentialsUpdate {
                    task: task.clone(),
                    text,
                });
            };
            // A fresh abort per call: Ctrl+C cancels the engine's wait (the
            // dropped request completes its own handshake), and the final
            // cancel re-call still goes through.
            let result = run_credentials_command(action, &abort, &mut report, |command| async {
                let per_call = harnx_runtime::utils::create_abort_signal();
                harnx_runtime::operator_tools::run_session_tool_command(
                    &snapshot, &per_call, command, true, &worker,
                )
                .await
            })
            .await;
            let (output, error) = match result {
                Ok(outcome) => (
                    outcome.output,
                    (outcome.failures > 0)
                        .then(|| format!("{} credential server(s) failed", outcome.failures)),
                ),
                Err(error) => (String::new(), Some(format!("{error:#}"))),
            };
            let _ = event_tx.send(TuiEvent::OperatorToolFinished {
                task: abort,
                output,
                error,
            });
        }));
    }

    /// Show a mid-run connect link while the credentials command's task is
    /// still the current one.
    pub(super) fn handle_credentials_update(&mut self, task: AbortSignal, text: String) {
        if !self
            .current_prompt_abort
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &task))
        {
            return;
        }
        if text.trim().is_empty() {
            return;
        }
        self.app
            .transcript
            .push(TranscriptItem::SystemText(text.clone()));
        crate::lifecycle::append_markdown_links(&mut self.app.transcript, &text);
        self.pin_transcript_to_bottom();
    }
}
