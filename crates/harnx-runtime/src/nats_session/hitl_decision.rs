//! Exact-request decisions and the tool-ID compatibility adapter.
use super::{
    fixed_admission::FixedAdmissionOutcome, hitl, ApprovalRef, HitlDecisionOutcome, NatsSession,
    PromptProgress, CONTROL_ACK_ATTEMPT_TIMEOUT, CONTROL_ACK_RETRY_DELAY, CONTROL_RECOVERY_TIMEOUT,
};
use crate::nats_worker::{request_control_command, ControlCommand};
use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;

impl NatsSession {
    /// Decide one exact approval generation, never a newer request with a reused ID.
    /// Already-applied answers preserve the original note; only the boolean answer
    /// determines a conflict. A terminal invocation or superseded request is stale.
    /// Pending and already-applied requests reactivate their original invocation.
    pub async fn decide_hitl_approval_for(
        &self,
        reference: &ApprovalRef,
        approved: bool,
        note: Option<String>,
    ) -> Result<HitlDecisionOutcome> {
        let entries = self.load_durable_entries().await?;
        let Some((prompt_seq, prompt_id)) = self.hitl_invocation_root(reference, &entries).await?
        else {
            return Ok(HitlDecisionOutcome::Stale);
        };
        let state =
            self.hitl_decision_state(reference, approved, prompt_seq, &prompt_id, &entries)?;
        if let Some(outcome) = state {
            if matches!(outcome, HitlDecisionOutcome::AlreadyApplied { .. }) {
                self.publish_activation(prompt_seq, None, None).await?;
            }
            return Ok(outcome);
        }
        // Like activate_pending_turn, but pinned to the validated original root.
        // Re-reading the session's current prompt here could activate a successor.
        self.publish_activation(prompt_seq, None, None).await?;
        let command = ControlCommand::HitlApprovalDecision {
            tool_call_id: reference.tool_call_id.clone(),
            approved,
            note,
            request_seq: Some(reference.request_seq),
        };
        let request = hitl::ApprovalRequest::from(reference);
        let deadline = tokio::time::Instant::now() + CONTROL_RECOVERY_TIMEOUT;
        loop {
            let _ = request_control_command(
                &self.client,
                &self.storage_key,
                &command,
                CONTROL_ACK_ATTEMPT_TIMEOUT,
            )
            .await;
            // An ACK isn't correlation proof, especially across worker upgrades.
            // Recover lost ACKs from the same exact request, even if it has since
            // completed or the worker has already parked at its next approval.
            let entries = self.load_durable_entries().await?;
            if let Some((decided, _)) = request.decision_with_note(&entries)? {
                return Ok(if decided == approved {
                    HitlDecisionOutcome::Applied
                } else {
                    HitlDecisionOutcome::Conflicting
                });
            }
            if let Some(outcome) =
                self.hitl_decision_state(reference, approved, prompt_seq, &prompt_id, &entries)?
            {
                return Ok(outcome);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for worker to apply HITL approval '{}'",
                    reference.tool_call_id
                );
            }
            tokio::time::sleep(CONTROL_ACK_RETRY_DELAY).await;
        }
    }

    /// Tool-ID compatibility API for interactive frontends. Select the current
    /// generation once, then use exact-request fencing. Historical same-answer
    /// retries still return true, including after completion, as before.
    pub async fn decide_hitl_approval(
        &self,
        tool_call_id: &str,
        approved: bool,
        note: Option<String>,
    ) -> Result<bool> {
        let entries = self.load_durable_entries().await?;
        let pending = crate::nats_worker::derive_pending_hitl_approvals(&entries)?;
        let Some(pending) = pending
            .iter()
            .find(|item| item.tool_call_id == tool_call_id)
        else {
            let decided = hitl::already_decided(&entries, tool_call_id, approved)?;
            if decided {
                if let Some(request) = hitl::latest_request(&entries, tool_call_id)? {
                    if let Some(reference) = self.hitl_reference(&request, &entries).await? {
                        // The exact API activates only a live, unsuperseded request.
                        let _ = self
                            .decide_hitl_approval_for(&reference, approved, note)
                            .await?;
                    }
                }
            }
            return Ok(decided);
        };
        let request = hitl::ApprovalRequest {
            tool_call_id,
            tool_round_seq: pending.tool_round_seq,
            request_seq: pending.request_seq,
        };
        let reference = self
            .hitl_reference(&request, &entries)
            .await?
            .context("pending HITL approval has no invocation admission")?;
        let outcome = self
            .decide_hitl_approval_for(&reference, approved, note)
            .await?;
        if matches!(
            outcome,
            HitlDecisionOutcome::Applied | HitlDecisionOutcome::AlreadyApplied { .. }
        ) {
            return Ok(true);
        }
        // Preserve legacy lost-ACK recovery if this request committed just before
        // the exact API's initial snapshot observed a newer request or terminal.
        let entries = self.load_durable_entries().await?;
        Ok(request.outcome(&entries, approved)? == Some(true))
    }

    /// None means still pending. Every resolved state is scoped to the root and
    /// latest approval, rather than merely the tool ID or physical request tail.
    fn hitl_decision_state(
        &self,
        reference: &ApprovalRef,
        approved: bool,
        prompt_seq: u64,
        prompt_id: &str,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<HitlDecisionOutcome>> {
        let progress = Self::invocation_progress_from_entries(
            &reference.invocation_id,
            prompt_id,
            prompt_seq,
            &self.session_id,
            entries,
        )?;
        Ok(match progress {
            PromptProgress::InputRequired(pending)
                if pending.approval_ref(&reference.invocation_id) == *reference =>
            {
                None
            }
            PromptProgress::Running {
                last_decision: Some(receipt),
            } if receipt.request == *reference => Some(if receipt.approved == approved {
                HitlDecisionOutcome::AlreadyApplied {
                    approved: receipt.approved,
                    note: receipt.note,
                }
            } else {
                HitlDecisionOutcome::Conflicting
            }),
            _ => Some(HitlDecisionOutcome::Stale),
        })
    }

    async fn hitl_invocation_root(
        &self,
        reference: &ApprovalRef,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<(u64, String)>> {
        let Some(admission) = self
            .metadata_store
            .admission(&self.storage_key, &reference.invocation_id)
            .await?
        else {
            return Ok(None);
        };
        let (seq, prompt_id) = if let Some(ticket) = admission.fixed_ticket {
            if ticket.storage_key() != self.storage_key
                || ticket.invocation_id() != reference.invocation_id
            {
                return Ok(None);
            }
            let FixedAdmissionOutcome::Admitted { prompt_sequence } = ticket.outcome(entries)?
            else {
                return Ok(None);
            };
            (prompt_sequence, ticket.prompt_id().to_owned())
        } else {
            let Some((seq, prompt_id, invocation_id)) = self
                .hitl_round_prompt(reference.tool_round_seq, entries)
                .await?
            else {
                return Ok(None);
            };
            if invocation_id != reference.invocation_id {
                return Ok(None);
            }
            (seq, prompt_id)
        };
        Ok((seq < reference.tool_round_seq).then_some((seq, prompt_id)))
    }

    async fn hitl_reference(
        &self,
        request: &hitl::ApprovalRequest<'_>,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<ApprovalRef>> {
        Ok(self
            .hitl_round_prompt(request.tool_round_seq, entries)
            .await?
            .map(|(_, _, invocation_id)| ApprovalRef {
                invocation_id,
                tool_round_seq: request.tool_round_seq,
                request_seq: request.request_seq,
                tool_call_id: request.tool_call_id.into(),
            }))
    }

    async fn hitl_round_prompt(
        &self,
        tool_round_seq: u64,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<(u64, String, String)>> {
        // Prompt bindings are immutable admission facts; queued steering rows may
        // appear after the round, so inspect only prompts preceding its ToolCalls.
        for (seq, entry) in entries
            .iter()
            .rev()
            .filter(|(seq, _)| *seq < tool_round_seq)
        {
            if let SessionLogEntry::Message {
                id: Some(id), role, ..
            } = entry
            {
                if role.is_user() {
                    if let Some(admission) = self
                        .metadata_store
                        .prompt_admission(&self.storage_key, id)
                        .await?
                    {
                        return Ok(Some((
                            *seq,
                            id.clone(),
                            admission.invocation_id.as_str().into(),
                        )));
                    }
                }
            }
        }
        Ok(None)
    }
}
