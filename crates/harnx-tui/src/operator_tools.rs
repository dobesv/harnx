//! Background operator commands keep the composer responsive to cancellation.

use crate::types::{TranscriptItem, Tui, TuiEvent};
use harnx_runtime::operator_tools::{OperatorToolCommand, OperatorToolReply};
use harnx_runtime::utils::AbortSignal;
use std::sync::Arc;

impl Tui {
    pub(super) fn start_operator_tool_command(&mut self, command: OperatorToolCommand) {
        self.retire_prompt_task();
        let abort = harnx_runtime::utils::create_abort_signal();
        self.current_prompt_abort = Some(abort.clone());
        // This is not an inference turn: Ctrl+C cancels the request, not the
        // durable session log. The normal task fence still rejects late output.
        self.active_remote_session = None;
        self.app.llm_busy = true;
        crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Working);
        self.refresh_input_chrome();
        let snapshot = Arc::new(harnx_runtime::config::ConfigLock::new(
            self.config.read().clone(),
        ));
        let worker = self.local_worker.clone();
        let event_tx = self.event_tx.clone();
        self.current_prompt_handle = Some(tokio::spawn(async move {
            let result = harnx_runtime::operator_tools::run_session_tool_command(
                &snapshot, &abort, command, false, &worker,
            )
            .await;
            let reply = result.unwrap_or_else(|error| OperatorToolReply {
                output: String::new(),
                error: Some(format!("{error:#}")),
            });
            let _ = event_tx.send(TuiEvent::OperatorToolFinished {
                task: abort,
                output: reply.output,
                error: reply.error,
            });
        }));
    }

    pub(super) async fn finish_operator_tool_command(
        &mut self,
        task: AbortSignal,
        output: String,
        error: Option<String>,
    ) {
        if !self
            .current_prompt_abort
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &task))
        {
            return;
        }
        if !task.aborted() && !output.is_empty() {
            self.app.transcript.push(TranscriptItem::SystemText(output));
            self.pin_transcript_to_bottom();
        }
        // Keep complete tool error/partial output visible before its diagnostic.
        self.finish_prompt_task(task, error).await;
    }
}
