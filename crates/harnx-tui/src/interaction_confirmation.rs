//! Durable middleware decisions are separate from legacy live HITL bool replies.

use crate::types::{ModalState, TranscriptItem, Tui};
use harnx_runtime::interactions::{derive_pending_interactions, InteractionDecisionOutcome};
use ratatui_textarea::TextArea;
use std::time::Instant;
use tokio::task::JoinHandle;

mod child;
mod child_decision;
mod decision;
mod observe;
pub(crate) use child::{answerable, pending_for_progress, ChildClient};
mod render;
pub(crate) use render::MIN_MODAL_HEIGHT;

pub(super) type Target = (String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InteractionKey {
    pub id: String,
    pub request_seq: u64,
}

/// Public projection only. Checkpoints and private middleware payloads never enter modal state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingConfirmation {
    pub key: InteractionKey,
    pub kind: String,
    pub message: String,
    pub pending_count: usize,
}

pub(super) fn pending_confirmation(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> anyhow::Result<Option<PendingConfirmation>> {
    let pending = derive_pending_interactions(entries)?;
    Ok(pending.first().map(|head| PendingConfirmation {
        key: InteractionKey {
            id: head.interaction_id.clone(),
            request_seq: head.request_seq,
        },
        kind: head.request.kind.clone(),
        message: head
            .request
            .payload
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| head.summary())
            .to_string(),
        pending_count: pending.len().saturating_sub(1),
    }))
}

#[derive(Clone, Debug)]
pub(super) struct InteractionModal {
    pub generation: u64,
    pub pending: PendingConfirmation,
    pub child: Option<child::ChildRequest>,
    pub note: TextArea<'static>,
    pub last_key_at: Instant,
    pub scroll: u16,
    pub submitting: bool,
    pub error: Option<String>,
}

impl InteractionModal {
    pub(super) fn same_request(&self, other: &Self) -> bool {
        (
            self.generation,
            &self.pending,
            &self.child,
            self.submitting,
            &self.error,
        ) == (
            other.generation,
            &other.pending,
            &other.child,
            other.submitting,
            &other.error,
        )
    }
}

#[derive(Default)]
pub(super) struct InteractionClient {
    children: ChildClient,
    target: Option<Target>,
    generation: u64,
    observer: Option<JoinHandle<()>>,
    pending: Option<PendingConfirmation>,
    accepted: Vec<InteractionKey>,
    read_error: Option<String>,
    decisions: Vec<JoinHandle<()>>,
}

impl Drop for InteractionClient {
    fn drop(&mut self) {
        if let Some(task) = self.observer.take() {
            task.abort();
        }
        for task in self.decisions.drain(..) {
            task.abort();
        }
    }
}

pub(crate) enum InteractionEvent {
    ChildDecided {
        request: child::ChildRequest,
        approved: bool,
        result: Result<child_decision::ChildDecisionOutcome, String>,
    },
    Snapshot {
        generation: u64,
        pending: Result<Option<PendingConfirmation>, String>,
    },
    Decided {
        generation: u64,
        target: Target,
        key: InteractionKey,
        result: Result<InteractionDecisionOutcome, String>,
    },
}

// qual:allow(coupling, oi) reason: "TUI behavior is split by concern, as in tool_confirmation.rs and input_modal.rs."
impl Tui {
    /// Unlike transcript monitoring this stays active while our own prompt task follows a turn.
    pub(super) fn sync_interaction_client(&mut self) {
        let target = self.session_activity_destination();
        if self.interactions.target != target {
            self.clear_interaction_modal();
            let generation = self.interactions.generation.wrapping_add(1);
            let decisions = std::mem::take(&mut self.interactions.decisions);
            self.interactions = InteractionClient {
                children: std::mem::take(&mut self.interactions.children),
                target: target.clone(),
                generation,
                decisions,
                observer: None,
                pending: None,
                accepted: Vec::new(),
                read_error: None,
            };
            self.interactions.observer = target.map(|target| {
                observe::spawn(
                    self.config.clone(),
                    self.event_tx.clone(),
                    target,
                    generation,
                )
            });
        }
        self.present_interaction();
        self.present_child_confirmation();
    }

    pub(super) fn handle_interaction_event(&mut self, event: InteractionEvent) {
        match event {
            InteractionEvent::ChildDecided {
                request,
                approved,
                result,
            } => {
                self.finish_child_decision(request, approved, result);
            }
            InteractionEvent::Snapshot {
                generation,
                pending,
            } => {
                if generation != self.interactions.generation {
                    return;
                }
                match pending {
                    Ok(pending) => {
                        self.interactions.pending = pending;
                        self.interactions.read_error = None;
                    }
                    Err(error) => {
                        if self.interactions.read_error.as_ref() != Some(&error) {
                            self.interaction_notice(format!(
                                "Cannot refresh middleware confirmations: {error}"
                            ));
                        }
                        self.interactions.read_error = Some(error);
                    }
                }
                self.reconcile_interaction_modal();
            }
            InteractionEvent::Decided {
                generation,
                target,
                key,
                result,
            } => {
                self.finish_interaction_decision(generation, target, key, result);
            }
        }
    }

    fn reconcile_interaction_modal(&mut self) {
        if matches!(&self.app.modal, Some(ModalState::ConfirmInteraction(state)) if state.child.is_some())
        {
            return;
        }
        if let Some(ModalState::ConfirmInteraction(state)) = &self.app.modal {
            let current = self
                .interactions
                .pending
                .as_ref()
                .map(|pending| &pending.key);
            if current != Some(&state.pending.key) {
                self.clear_interaction_modal();
            }
        }
        if let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() {
            if let Some(pending) = &self.interactions.pending {
                state.pending = pending.clone();
            }
        }
        self.present_interaction();
    }

    fn present_interaction(&mut self) {
        if self.app.modal.is_some() || self.has_root_cancellation() {
            return;
        }
        let Some(pending) = self.interactions.pending.clone() else {
            return;
        };
        if self.interactions.accepted.contains(&pending.key) {
            return;
        }
        self.app.modal = Some(ModalState::ConfirmInteraction(Box::new(InteractionModal {
            generation: self.interactions.generation,
            pending,
            child: None,
            note: TextArea::default(),
            last_key_at: Instant::now(),
            scroll: 0,
            submitting: false,
            error: None,
        })));
        crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Blocked);
    }

    fn clear_interaction_modal(&mut self) {
        if matches!(self.app.modal, Some(ModalState::ConfirmInteraction(_))) {
            self.app.modal = None;
            if self.app.llm_busy {
                crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Working);
            }
        }
    }

    fn interaction_notice(&mut self, message: String) {
        self.app.show_footer_notice(&message);
        self.app.transcript.push(TranscriptItem::ErrorText(message));
        self.pin_transcript_to_bottom();
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod live_harness;
#[cfg(test)]
mod live_tests;
