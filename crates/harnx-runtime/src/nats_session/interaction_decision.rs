//! Exact interaction decision client API for NatsSession.

use super::{
    NatsSession, CONTROL_ACK_ATTEMPT_TIMEOUT, CONTROL_ACK_RETRY_DELAY, CONTROL_RECOVERY_TIMEOUT,
};
use crate::interactions::pending::{
    entries_for_round, existing_response, find_request_by_id, round_is_terminal,
};
use crate::interactions::InteractionDecisionOutcome;
use crate::nats_worker::{control_subject, ControlCommand};
use anyhow::Result;
use harnx_core::interaction::InteractionResponse;
use harnx_core::session::SessionLogEntry;
use harnx_core::session_reconstruct::apply_log_mutations_nats;

impl NatsSession {
    /// Submit an exact interaction decision over the control subject and await confirmation.
    pub async fn decide_interaction(
        &self,
        interaction_id: &str,
        request_seq: u64,
        response: &InteractionResponse,
    ) -> Result<InteractionDecisionOutcome> {
        let command = ControlCommand::InteractionResponse {
            interaction_id: interaction_id.to_string(),
            request_seq,
            response: response.clone(),
        };

        let payload: bytes::Bytes = command.to_bytes()?.into();
        let subject = control_subject(&self.storage_key);
        let deadline = tokio::time::Instant::now() + CONTROL_RECOVERY_TIMEOUT;
        loop {
            // A fast resumed call can finish before the client reads its receipt.
            // Return the lease holder's decision, not a later terminal projection.
            if let Ok(Ok(reply)) = tokio::time::timeout(
                CONTROL_ACK_ATTEMPT_TIMEOUT,
                self.client.request(subject.clone(), payload.clone()),
            )
            .await
            {
                if let Ok(outcome) = serde_json::from_slice(&reply.payload) {
                    return Ok(outcome);
                }
            }

            let entries = self.load_durable_entries().await?;
            let effective = apply_log_mutations_nats(&entries)?;

            if let Some(outcome) =
                evaluate_interaction_outcome(&effective, interaction_id, request_seq, response)
            {
                return Ok(outcome);
            }

            if tokio::time::Instant::now() >= deadline {
                if find_request_by_id(&effective, interaction_id).is_none() {
                    return Ok(InteractionDecisionOutcome::Stale);
                }
                anyhow::bail!(
                    "timed out waiting for worker to apply interaction response for '{interaction_id}'"
                );
            }
            tokio::time::sleep(CONTROL_ACK_RETRY_DELAY).await;
        }
    }
}

fn evaluate_interaction_outcome(
    entries: &[(u64, SessionLogEntry)],
    interaction_id: &str,
    request_seq: u64,
    response: &InteractionResponse,
) -> Option<InteractionDecisionOutcome> {
    let effective = apply_log_mutations_nats(entries).ok()?;
    let requested = find_request_by_id(&effective, interaction_id)?;
    if requested.request_seq != request_seq {
        return Some(InteractionDecisionOutcome::WrongSequence);
    }
    let round_seq = requested.tool_round_seq;
    if round_is_terminal(&effective, round_seq) {
        return Some(InteractionDecisionOutcome::AfterComplete);
    }
    let round_entries = entries_for_round(&effective, round_seq);
    if let Some(existing) = existing_response(round_entries, interaction_id, request_seq) {
        return Some(if existing == *response {
            InteractionDecisionOutcome::Applied
        } else {
            InteractionDecisionOutcome::Conflicting
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::interaction::InteractionStep;
    use serde_json::json;

    // qual:allow(dry, duplicate) reason: "independent unit vs integration requested sample"
    fn test_req(seq: u64, round_seq: u64, id: &str) -> (u64, SessionLogEntry) {
        (
            seq,
            SessionLogEntry::InteractionRequested {
                tool_round_seq: round_seq,
                tool_call_id: "call-1".into(),
                interaction_id: id.into(),
                producer: "test".into(),
                step_path: vec![InteractionStep {
                    middleware: "test".into(),
                    key: "1".into(),
                }],
                key: "1".into(),
                kind: "confirm".into(),
                payload: json!({}),
                summary: "summary".into(),
                chain_version: "v1".into(),
                checkpoint: "cp".into(),
                fence_token: 10,
            },
        )
    }

    #[test]
    fn test_evaluate_interaction_outcome() {
        let resp = InteractionResponse {
            action: "approve".into(),
            data: serde_json::Map::new(),
            note: None,
        };

        // Unknown interaction -> None (keep polling)
        assert_eq!(evaluate_interaction_outcome(&[], "int-1", 2, &resp), None);

        // Wrong sequence -> WrongSequence immediately
        let entries = vec![test_req(2, 1, "int-1")];
        assert_eq!(
            evaluate_interaction_outcome(&entries, "int-1", 999, &resp),
            Some(InteractionDecisionOutcome::WrongSequence)
        );

        // Round terminal -> AfterComplete immediately
        let term_entries = vec![
            (
                1,
                SessionLogEntry::ToolCalls {
                    text: String::new(),
                    thought: None,
                    calls: vec![],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            test_req(2, 1, "int-1"),
            (
                3,
                SessionLogEntry::ToolResults {
                    results: vec![],
                    handoff_selection: None,
                    timestamp: None,
                },
            ),
        ];
        assert_eq!(
            evaluate_interaction_outcome(&term_entries, "int-1", 2, &resp),
            Some(InteractionDecisionOutcome::AfterComplete)
        );

        // Pending unapplied -> None
        let open_entries = vec![
            (
                1,
                SessionLogEntry::ToolCalls {
                    text: String::new(),
                    thought: None,
                    calls: vec![],
                    timestamp: None,
                    fence_token: None,
                },
            ),
            test_req(2, 1, "int-1"),
        ];
        assert_eq!(
            evaluate_interaction_outcome(&open_entries, "int-1", 2, &resp),
            None
        );

        // Applied response -> Applied
        let applied_entries = vec![
            open_entries[0].clone(),
            open_entries[1].clone(),
            (
                3,
                SessionLogEntry::InteractionResponse {
                    request_seq: 2,
                    interaction_id: "int-1".into(),
                    tool_call_id: "call-1".into(),
                    response: resp.clone(),
                    fence_token: 10,
                },
            ),
        ];
        assert_eq!(
            evaluate_interaction_outcome(&applied_entries, "int-1", 2, &resp),
            Some(InteractionDecisionOutcome::Applied)
        );

        // Conflicting response -> Conflicting
        let diff_resp = InteractionResponse {
            action: "deny".into(),
            data: serde_json::Map::new(),
            note: None,
        };
        assert_eq!(
            evaluate_interaction_outcome(&applied_entries, "int-1", 2, &diff_resp),
            Some(InteractionDecisionOutcome::Conflicting)
        );
    }
}
