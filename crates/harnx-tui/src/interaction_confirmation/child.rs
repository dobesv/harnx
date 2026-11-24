//! Parent-row confirmations retain the displayed child's exact request, not the active session.

use super::{InteractionKey, InteractionModal, PendingConfirmation, Target};
use crate::types::{ModalState, SubAgentInvocationProgress, SubAgentStatus, TranscriptItem, Tui};
use harnx_core::pending_interaction::{PendingInteractionRequest, PendingInteractionSummary};
use ratatui_textarea::TextArea;
use std::collections::VecDeque;
use std::time::Instant;
use tokio::task::JoinHandle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChildRequest {
    pub origin: Option<Target>,
    pub invocation_id: String,
    pub summary: PendingInteractionSummary,
}

impl ChildRequest {
    pub(super) fn same_request(&self, other: &Self) -> bool {
        self.origin == other.origin
            && self.invocation_id == other.invocation_id
            && self.summary.agent == other.summary.agent
            && self.summary.cluster == other.summary.cluster
            && self.summary.session_id == other.summary.session_id
            && self.summary.request == other.summary.request
    }

    pub(super) fn target(&self) -> Target {
        (
            harnx_core::session_identity::session_key(
                Some(&self.summary.agent),
                &self.summary.session_id,
            ),
            self.summary.cluster.clone(),
        )
    }

    fn confirmation(&self) -> PendingConfirmation {
        let key = match &self.summary.request {
            PendingInteractionRequest::LegacyApproval { request } => InteractionKey {
                id: request.tool_call_id.clone(),
                request_seq: request.request_seq,
            },
            PendingInteractionRequest::Interaction {
                interaction_id,
                request_seq,
                ..
            } => InteractionKey {
                id: interaction_id.clone(),
                request_seq: *request_seq,
            },
        };
        PendingConfirmation {
            key,
            kind: "confirm".into(),
            message: format!(
                "Child {} / {} on {}\nTool: {}\n{}",
                self.summary.agent,
                self.summary.session_id,
                self.summary.cluster,
                self.summary.tool_name,
                self.summary.message,
            ),
            payload: serde_json::Value::Null,
            pending_count: self.summary.pending_count,
        }
    }
}

#[derive(Default)]
pub(crate) struct ChildClient {
    queue: VecDeque<ChildRequest>,
    pub(super) decisions: Vec<JoinHandle<()>>,
    #[cfg(test)]
    pub(super) decision_override: Option<super::child_decision::TestDecisionFn>,
}

impl Drop for ChildClient {
    fn drop(&mut self) {
        for task in self.decisions.drain(..) {
            task.abort();
        }
    }
}

pub(crate) fn pending_for_progress<'a>(
    status: &SubAgentStatus,
    progress: Option<&'a SubAgentInvocationProgress>,
) -> Option<&'a PendingInteractionSummary> {
    if matches!(
        status,
        SubAgentStatus::Completed | SubAgentStatus::Failed | SubAgentStatus::Cancelled
    ) {
        return None;
    }
    let progress = progress.filter(|progress| !progress.is_terminal())?;
    progress.snapshot.pending_interaction.as_ref()
}

pub(crate) fn answerable(summary: &PendingInteractionSummary) -> bool {
    match &summary.request {
        PendingInteractionRequest::LegacyApproval { .. } => true,
        PendingInteractionRequest::Interaction {
            interaction_kind, ..
        } => interaction_kind == "confirm",
    }
}

fn request_from_row(item: &TranscriptItem, origin: Option<Target>) -> Option<ChildRequest> {
    let TranscriptItem::SubAgentSession {
        status, progress, ..
    } = item
    else {
        return None;
    };
    let summary = pending_for_progress(status, progress.as_ref())?;
    Some(ChildRequest {
        origin,
        invocation_id: progress.as_ref()?.snapshot.invocation_id.clone(),
        summary: summary.clone(),
    })
}

// qual:allow(coupling, oi) reason: "Parent-row navigation and modal queue extend the shared TUI."
impl Tui {
    pub(super) fn interrupt_child_confirmation(&mut self) -> bool {
        let Some(ModalState::ConfirmInteraction(state)) = &self.app.modal else {
            return false;
        };
        let Some(child) = &state.child else {
            return false;
        };
        let target = child.target();
        self.clear_interaction_modal();
        self.exit_after_cancel = false;
        self.start_cancellation(target.0, target.1);
        true
    }

    pub(crate) fn handle_child_answer_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        self.app.modal.is_none()
            && key.code == KeyCode::Char('a')
            && key.modifiers == KeyModifiers::NONE
            && (self.app.transcript_focus.is_some() || !self.app.subagent_view_stack.is_empty())
            && self.answer_selected_child()
    }

    /// Only root rows and the first opened child's own header are eligible. Never its descendants.
    pub(crate) fn answer_selected_child(&mut self) -> bool {
        if self.app.archive_browser.is_some()
            || self.app.doc_view.is_some()
            || self.app.detail_view_entry.is_some()
        {
            return false;
        }
        let origin = self.session_activity_destination();
        let selected = match self.app.subagent_view_stack.as_slice() {
            [] => self
                .app
                .transcript_focus
                .and_then(|focus| self.app.transcript.get(focus))
                .and_then(|item| request_from_row(item, origin.clone())),
            [view] => self
                .app
                .transcript
                .iter()
                .filter_map(|item| request_from_row(item, origin.clone()))
                .find(|request| {
                    view.progress.as_ref().is_some_and(|progress| {
                        progress.snapshot.invocation_id == request.invocation_id
                    })
                }),
            _ => None,
        };
        let Some(request) = selected else {
            return false;
        };
        if !answerable(&request.summary) {
            self.app.show_footer_notice(
                "This interaction kind cannot be answered here; open the child session.",
            );
            return true;
        }
        self.queue_child_confirmation(request);
        true
    }

    pub(super) fn queue_child_confirmation(&mut self, request: ChildRequest) {
        if self
            .interactions
            .children
            .queue
            .iter()
            .any(|queued| queued.same_request(&request))
            || matches!(&self.app.modal, Some(ModalState::ConfirmInteraction(state)) if state.child.as_ref().is_some_and(|active| active.same_request(&request)))
        {
            return;
        }
        self.interactions.children.queue.push_back(request);
        self.present_interaction();
        self.present_child_confirmation();
        if !self.interactions.children.queue.is_empty() {
            self.app
                .show_footer_notice("Child confirmation queued behind the current modal.");
        }
    }

    pub(super) fn child_request_is_current(&self, request: &ChildRequest) -> bool {
        self.app
            .transcript
            .iter()
            .filter_map(|item| request_from_row(item, self.session_activity_destination()))
            .any(|current| current.same_request(request))
    }

    pub(super) fn present_child_confirmation(&mut self) {
        if self.app.modal.is_some()
            || self.app.pending_confirm_reply.is_some()
            || self.has_root_cancellation()
        {
            return;
        }
        while let Some(request) = self.interactions.children.queue.pop_front() {
            if !self.child_request_is_current(&request) || !answerable(&request.summary) {
                continue;
            }
            self.app.modal = Some(ModalState::ConfirmInteraction(Box::new(InteractionModal {
                generation: self.interactions.generation,
                pending: request.confirmation(),
                child: Some(request),
                a2ui: None,
                note: TextArea::default(),
                last_key_at: Instant::now(),
                scroll: 0,
                submitting: false,
                error: None,
            })));
            return;
        }
    }

    pub(super) fn clear_child_pending(&mut self, request: &ChildRequest) {
        for item in &mut self.app.transcript {
            if request_from_row(item, request.origin.clone())
                .is_some_and(|current| current.same_request(request))
            {
                if let TranscriptItem::SubAgentSession {
                    progress: Some(progress),
                    ..
                } = item
                {
                    progress.snapshot.pending_interaction = None;
                }
            }
        }
        for view in &mut self.app.subagent_view_stack {
            if let Some(progress) = &mut view.progress {
                if progress.snapshot.invocation_id == request.invocation_id
                    && progress
                        .snapshot
                        .pending_interaction
                        .as_ref()
                        .is_some_and(|summary| summary.request == request.summary.request)
                {
                    progress.snapshot.pending_interaction = None;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "child_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "child_live_tests.rs"]
mod live_tests;
