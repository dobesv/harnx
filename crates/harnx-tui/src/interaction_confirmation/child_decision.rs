use super::child::ChildRequest;
use super::decision::decision_response;
use super::InteractionEvent;
use crate::types::{ModalState, Tui, TuiEvent};
use harnx_core::pending_interaction::PendingInteractionRequest;
use harnx_runtime::interactions::InteractionDecisionOutcome;
use harnx_runtime::nats_session::HitlDecisionOutcome;

pub(crate) enum ChildDecisionOutcome {
    Legacy(HitlDecisionOutcome),
    Middleware(InteractionDecisionOutcome),
}

#[cfg(test)]
pub(super) type TestDecisionFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ChildDecisionOutcome, String>> + Send>,
>;
#[cfg(test)]
pub(super) type TestDecisionFn =
    std::sync::Arc<dyn Fn(ChildRequest, bool, String) -> TestDecisionFuture + Send + Sync>;

// qual:allow(coupling, oi) reason: "Exact child decisions extend the shared TUI without using foreground legacy replies."
impl Tui {
    pub(super) fn submit_child_interaction(&mut self, approved: bool) {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        let Some(request) = state.child.clone() else {
            return;
        };
        if state.submitting {
            return;
        }
        state.submitting = true;
        state.error = None;
        let note = state.note.lines().join("\n").trim().to_string();
        let config = self.config.clone();
        let local_worker = self.local_worker.clone();
        let event_tx = self.event_tx.clone();
        #[cfg(test)]
        let decision_override = self.interactions.children.decision_override.clone();
        self.interactions
            .children
            .decisions
            .retain(|task| !task.is_finished());
        self.interactions
            .children
            .decisions
            .push(tokio::spawn(async move {
                let target = request.target();
                let decision = async {
                    let session = crate::remote_session::nats_session_for_target(
                        &config,
                        &local_worker,
                        target.0,
                        target.1,
                    )
                    .await?;
                    decide(&session, &request.summary.request, approved, &note).await
                };
                #[cfg(test)]
                let result = if let Some(decide) = decision_override {
                    decide(request.clone(), approved, note.clone()).await
                } else {
                    decision
                        .await
                        .map_err(|error: anyhow::Error| format!("{error:#}"))
                };
                #[cfg(not(test))]
                let result = decision
                    .await
                    .map_err(|error: anyhow::Error| format!("{error:#}"));
                let _ = event_tx.send(TuiEvent::Interaction(InteractionEvent::ChildDecided {
                    request: request.clone(),
                    approved,
                    result,
                }));
            }));
    }

    pub(super) fn finish_child_decision(
        &mut self,
        request: ChildRequest,
        approved: bool,
        result: Result<ChildDecisionOutcome, String>,
    ) {
        if request.origin != self.session_activity_destination() {
            return;
        }
        let matches_modal = matches!(&self.app.modal, Some(ModalState::ConfirmInteraction(state)) if state.child.as_ref().is_some_and(|child| child.same_request(&request)));
        // An old reply must never clear a newer generation, even if the provider reused its tool ID.
        if !self.child_request_is_current(&request) {
            if matches_modal {
                self.clear_interaction_modal();
            }
            return;
        }
        let (settled, notice) = outcome_notice(approved, result);

        let message = format!(
            "Child {} / {} on {}: {notice}",
            request.summary.agent, request.summary.session_id, request.summary.cluster
        );
        self.app.show_footer_notice(&message);
        self.app
            .transcript
            .push(crate::types::TranscriptItem::StatusLine(message));
        if settled {
            self.clear_child_pending(&request);
            if matches_modal {
                self.clear_interaction_modal();
            }
        } else if matches_modal {
            if let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() {
                state.submitting = false;
                state.error = Some(notice);
            }
        }
        self.interactions
            .children
            .decisions
            .retain(|task| !task.is_finished());
        self.present_interaction();
        self.present_child_confirmation();
    }
}

fn outcome_notice(approved: bool, result: Result<ChildDecisionOutcome, String>) -> (bool, String) {
    use ChildDecisionOutcome::{Legacy, Middleware};
    use HitlDecisionOutcome as LegacyDecision;
    use InteractionDecisionOutcome as Interaction;
    let answer = |approved| if approved { "approved" } else { "denied" };
    match result {
        Ok(Legacy(LegacyDecision::Applied) | Middleware(Interaction::Applied)) => {
            (true, format!("response applied: {}", answer(approved)))
        }
        Ok(Legacy(LegacyDecision::AlreadyApplied {
            approved: recorded,
            note,
        })) => {
            let label = if approved == recorded {
                "already applied"
            } else {
                "answered differently elsewhere"
            };
            let note = note.map(|note| format!(" ({note})")).unwrap_or_default();
            (true, format!("{label}: {}{note}", answer(recorded)))
        }
        Ok(Middleware(Interaction::AlreadyApplied)) => {
            (true, format!("already applied: {}", answer(approved)))
        }
        Ok(Legacy(LegacyDecision::Conflicting)) => (
            true,
            format!("answered differently elsewhere: {}", answer(!approved)),
        ),
        Ok(Middleware(Interaction::Conflicting)) => (
            true,
            "answered differently elsewhere (recorded answer unavailable; open child session)"
                .into(),
        ),
        Ok(
            Legacy(LegacyDecision::Stale)
            | Middleware(
                Interaction::Stale | Interaction::AfterComplete | Interaction::WrongSequence,
            ),
        ) => (true, "this request is no longer pending".into()),
        Ok(Middleware(Interaction::LeaseLost)) => (
            false,
            "worker ownership changed; retry the response or open child session".into(),
        ),
        Err(error) => (
            false,
            format!("response failed: {error}; open child session (no legacy fallback)"),
        ),
    }
}

async fn decide(
    session: &harnx_runtime::NatsSession,
    request: &PendingInteractionRequest,
    approved: bool,
    note: &str,
) -> anyhow::Result<ChildDecisionOutcome> {
    match request {
        PendingInteractionRequest::LegacyApproval { request } => session
            .decide_hitl_approval_for(
                request,
                approved,
                (!note.is_empty()).then(|| note.to_string()),
            )
            .await
            .map(ChildDecisionOutcome::Legacy),
        PendingInteractionRequest::Interaction {
            interaction_id,
            request_seq,
            ..
        } => session
            .decide_interaction(
                interaction_id,
                *request_seq,
                &decision_response(approved, note.to_string()),
            )
            .await
            .map(ChildDecisionOutcome::Middleware),
    }
}
