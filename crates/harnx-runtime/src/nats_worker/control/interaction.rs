//! Interaction response control handler.

use super::{AppliedHitlDecision, SessionControlHandler};
use crate::interactions::{accept_interaction_response, InteractionDecisionOutcome};
use harnx_core::interaction::InteractionResponse;

impl SessionControlHandler {
    pub(super) async fn apply_interaction_response(
        &self,
        interaction_id: &str,
        request_seq: u64,
        response: &InteractionResponse,
        reply: Option<async_nats::Subject>,
    ) {
        let outcome = match accept_interaction_response(
            &self.backend,
            &self.lease,
            interaction_id,
            request_seq,
            response,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                log::warn!("failed to apply interaction response: {error:#}");
                return;
            }
        };

        if outcome == InteractionDecisionOutcome::Applied {
            self.publish_updated().await;
            let _ = self.hitl_decision_tx.send(AppliedHitlDecision {
                tool_call_id: "interaction".into(),
                approved: true,
                note: None,
            });
        }
        // A stale subscriber must not beat the current lease holder's reply.
        if outcome != InteractionDecisionOutcome::LeaseLost {
            self.publish_control_reply(reply, &outcome, "interaction decision")
                .await;
        }
    }
}
