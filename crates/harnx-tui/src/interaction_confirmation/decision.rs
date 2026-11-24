use super::{InteractionEvent, InteractionKey, InteractionModal, Target};
use crate::types::{ModalState, Tui, TuiEvent};
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::interactions::InteractionDecisionOutcome;
use harnx_runtime::local_orchestrator::LocalWorkerSupervisor;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

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
        if state.submitting {
            return;
        }
        if let Some(form) = &state.a2ui {
            let action = if approve {
                form.primary_action()
            } else {
                Some("deny".to_string())
            };
            if let Some(action) = action {
                self.submit_a2ui_interaction(action);
            }
            return;
        }
        // `url` interactions answer continue/cancel; only `confirm` approves.
        let Some(action) = resolve_decision_action(&state.pending.kind, approve) else {
            return;
        };
        // Decision 6: a `url` response stores the action alone — no note.
        let note = if state.pending.kind == "url" {
            String::new()
        } else {
            state.note.lines().join("\n")
        };
        let response = decision_response_for(action, note);
        self.dispatch_modal_decision(response);
    }

    pub(crate) fn submit_a2ui_interaction(&mut self, action: String) {
        if matches!(&self.app.modal, Some(ModalState::ConfirmInteraction(state)) if state.child.is_some())
        {
            return;
        }
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        if state.submitting {
            return;
        }
        let response_data = match build_a2ui_data(state, &action) {
            Ok(data) => data,
            Err(err) => {
                state.error = Some(err);
                return;
            }
        };
        let note = state.note.lines().join("\n");
        let response = InteractionResponse {
            action,
            data: response_data,
            note: (!note.trim().is_empty()).then_some(note),
        };
        self.dispatch_modal_decision(response);
    }

    fn dispatch_modal_decision(&mut self, response: InteractionResponse) {
        let target = match self.interactions.check_target() {
            Ok(target) => target,
            Err(err) => {
                if let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() {
                    state.error = Some(err);
                }
                return;
            }
        };
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        state.submitting = true;
        state.error = None;
        let key = state.pending.key.clone();
        let generation = state.generation;
        let config = self.config.clone();
        let local_worker = self.local_worker.clone();
        let event_tx = self.event_tx.clone();
        let ctx = DecisionTaskContext {
            config,
            local_worker,
            event_tx,
            target,
            key,
            generation,
        };
        self.interactions
            .decisions
            .retain(|task| !task.is_finished());
        self.interactions
            .decisions
            .push(spawn_decision_task(ctx, response));
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

fn build_a2ui_data(
    state: &InteractionModal,
    action: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    if let Some(form) = &state.a2ui {
        let values = form.values();
        harnx_core::interaction::response::build_data(&state.pending.payload, action, &values)
    } else {
        Ok(serde_json::Map::new())
    }
}

struct DecisionTaskContext {
    config: GlobalConfig,
    local_worker: Arc<Mutex<Option<LocalWorkerSupervisor>>>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    target: Target,
    key: InteractionKey,
    generation: u64,
}

fn spawn_decision_task(ctx: DecisionTaskContext, response: InteractionResponse) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = async {
            let session = crate::remote_session::nats_session_for_target(
                &ctx.config,
                &ctx.local_worker,
                ctx.target.0.clone(),
                ctx.target.1.clone(),
            )
            .await?;
            session
                .decide_interaction(&ctx.key.id, ctx.key.request_seq, &response)
                .await
        }
        .await
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        let _ = ctx
            .event_tx
            .send(TuiEvent::Interaction(InteractionEvent::Decided {
                generation: ctx.generation,
                target: ctx.target,
                key: ctx.key,
                result,
            }));
    })
}

pub(super) fn decision_response(approve: bool, note: String) -> InteractionResponse {
    decision_response_for(if approve { "approve" } else { "deny" }, note)
}

/// Map an approval-impulse and a pending kind to the durable action. Confirm
/// answers approve/deny, url answers continue/cancel (decision 6); an
/// affirmative press on any other kind is refused (deny stays available).
pub(super) fn resolve_decision_action(kind: &str, approve: bool) -> Option<&'static str> {
    match (approve, kind) {
        (true, "confirm") | (false, "confirm") => Some(if approve { "approve" } else { "deny" }),
        (true, "url") => Some("continue"),
        (false, "url") => Some("cancel"),
        (true, _) => None,
        (false, _) => Some("deny"),
    }
}

/// Build the durable response for a resolved action. `confirm` answers
/// approve/deny, `url` answers continue/cancel (decision 6); `data` stays
/// empty for both kinds.
pub(super) fn decision_response_for(action: &str, note: String) -> InteractionResponse {
    InteractionResponse {
        action: action.to_string(),
        data: Default::default(),
        note: (!note.trim().is_empty()).then_some(note),
    }
}

fn decision_error(result: Result<InteractionDecisionOutcome, String>) -> Option<String> {
    use InteractionDecisionOutcome::{
        AfterComplete, AlreadyApplied, Applied, Conflicting, InvalidResponse, LeaseLost, Stale,
        WrongSequence,
    };
    match result {
        Ok(Applied | AlreadyApplied) => None,
        Ok(Conflicting) => Some("conflicting: another response was already accepted".into()),
        Ok(WrongSequence) => Some("wrong_sequence: request sequence does not match".into()),
        Ok(Stale) => Some("stale: request is no longer pending".into()),
        Ok(AfterComplete) => Some("after_complete: round has ended or was cancelled".into()),
        Ok(InvalidResponse) => {
            Some("invalid_response: not a declared action or valid data for this surface".into())
        }
        Ok(LeaseLost) => Some("lease_lost: worker ownership changed; retry the response".into()),
        Err(error) => Some(error),
    }
}
