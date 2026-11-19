use super::{InteractionEvent, InteractionKey, Target};
use crate::types::{ModalState, Tui, TuiEvent};
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::interactions::InteractionDecisionOutcome;

// qual:allow(coupling, oi) reason: "Decision handling extends the shared TUI, following the existing per-concern modules."
impl Tui {
    pub(crate) fn submit_interaction(&mut self, approve: bool) {
        if matches!(&self.app.modal, Some(ModalState::ConfirmInteraction(state)) if state.child.is_some())
        {
            self.submit_child_interaction(approve);
            return;
        }
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        if state.submitting || (approve && state.pending.kind != "confirm") {
            return;
        }
        if let Some(error) = &self.interactions.read_error {
            state.error = Some(format!("Cannot refresh request: {error}"));
            return;
        }
        let Some(target) = self.interactions.target.clone() else {
            state.error = Some("Session is no longer selected".into());
            return;
        };
        state.submitting = true;
        state.error = None;
        let key = state.pending.key.clone();
        let generation = state.generation;
        let response = decision_response(approve, state.note.lines().join("\n"));
        let config = self.config.clone();
        let local_worker = self.local_worker.clone();
        let event_tx = self.event_tx.clone();
        self.interactions
            .decisions
            .retain(|task| !task.is_finished());
        self.interactions.decisions.push(tokio::spawn(async move {
            let result = async {
                let session = crate::remote_session::nats_session_for_target(
                    &config,
                    &local_worker,
                    target.0.clone(),
                    target.1.clone(),
                )
                .await?;
                session
                    .decide_interaction(&key.id, key.request_seq, &response)
                    .await
            }
            .await
            .map_err(|error: anyhow::Error| format!("{error:#}"));
            let _ = event_tx.send(TuiEvent::Interaction(InteractionEvent::Decided {
                generation,
                target,
                key,
                result,
            }));
        }));
    }

    pub(super) fn finish_interaction_decision(
        &mut self,
        generation: u64,
        target: Target,
        key: InteractionKey,
        result: Result<InteractionDecisionOutcome, String>,
    ) {
        let error = decision_error(result);
        // Report rejected decisions even when a newer snapshot already replaced the modal.
        if let Some(error) = &error {
            self.interaction_notice(format!(
                "Interaction '{}' (request {}, session {} on {}) response rejected: {error}",
                key.id, key.request_seq, target.0, target.1,
            ));
        }
        if generation != self.interactions.generation {
            return;
        }
        self.interactions
            .decisions
            .retain(|task| !task.is_finished());
        if error.is_none() {
            self.interactions.accepted.push(key.clone());
        }
        if let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() {
            if state.child.is_none() && state.pending.key == key && state.generation == generation {
                state.submitting = false;
                state.error = error.clone();
                if error.is_none() {
                    self.clear_interaction_modal();
                }
            }
        }
        self.present_interaction();
    }
}

pub(super) fn decision_response(approve: bool, note: String) -> InteractionResponse {
    InteractionResponse {
        action: if approve { "approve" } else { "deny" }.to_string(),
        data: Default::default(),
        note: (!note.trim().is_empty()).then_some(note),
    }
}

fn decision_error(result: Result<InteractionDecisionOutcome, String>) -> Option<String> {
    use InteractionDecisionOutcome::{
        AfterComplete, AlreadyApplied, Applied, Conflicting, LeaseLost, Stale, WrongSequence,
    };
    match result {
        Ok(Applied | AlreadyApplied) => None,
        Ok(Conflicting) => Some("conflicting: another response was already accepted".into()),
        Ok(WrongSequence) => Some("wrong_sequence: request sequence does not match".into()),
        Ok(Stale) => Some("stale: request is no longer pending".into()),
        Ok(AfterComplete) => Some("after_complete: round has ended or was cancelled".into()),
        Ok(LeaseLost) => Some("lease_lost: worker ownership changed; retry the response".into()),
        Err(error) => Some(error),
    }
}
