//! State transitions for interactive tool-use confirmation modals.

use crate::types::{ModalState, ToolConfirmationEvent, TranscriptItem, Tui};

impl Tui {
    pub(super) fn handle_tool_confirmation_event(&mut self, event: ToolConfirmationEvent) {
        match event {
            ToolConfirmationEvent::Show {
                confirmation_id,
                tool_name,
                input_preview,
                reason,
                reply,
            } => self.show_tool_confirmation(
                confirmation_id,
                tool_name,
                input_preview,
                reason,
                reply,
            ),
            ToolConfirmationEvent::Dismiss { confirmation_id } => {
                self.dismiss_tool_confirmation(confirmation_id);
            }
        }
    }

    fn show_tool_confirmation(
        &mut self,
        confirmation_id: u64,
        tool_name: String,
        input_preview: String,
        reason: Option<String>,
        reply: std::sync::mpsc::Sender<bool>,
    ) {
        if self.app.modal.is_some() || self.app.pending_confirm_reply.is_some() {
            let _ = reply.send(false);
            self.app.transcript.push(TranscriptItem::SystemText(
                "⚠ Tool confirmation denied because another modal is already open.".to_string(),
            ));
            self.pin_transcript_to_bottom();
            return;
        }

        // A blocked tool-eval thread is waiting on `reply`. Show the native
        // modal and remember the channel; answering the modal sends the
        // decision back.
        self.app.pending_confirm_reply = Some(reply);
        self.app.pending_confirm_id = Some(confirmation_id);
        self.app.modal = Some(ModalState::ConfirmToolUse {
            tool_name,
            input_preview,
            reason,
        });
    }

    fn dismiss_tool_confirmation(&mut self, confirmation_id: u64) {
        if self.app.pending_confirm_id != Some(confirmation_id) {
            return;
        }
        self.resolve_tool_confirm(false);
        self.app.transcript.push(TranscriptItem::SystemText(
            "⚠ Tool confirmation expired or was cancelled.".to_string(),
        ));
        self.pin_transcript_to_bottom();
    }

    /// Resolve an in-flight tool-use confirmation: send the decision to the
    /// blocked tool-eval thread and dismiss the modal.
    pub(super) fn resolve_tool_confirm(&mut self, allow: bool) {
        if let Some(reply) = self.app.pending_confirm_reply.take() {
            let _ = reply.send(allow);
        }
        self.app.pending_confirm_id = None;
        self.app.modal = None;
    }
}
