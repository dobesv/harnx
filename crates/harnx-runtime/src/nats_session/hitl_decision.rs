//! Exact-request decisions and the tool-ID compatibility adapter.
mod outcome;
use super::{
    fixed_admission::{FixedAdmissionOutcome, FixedAdmissionTicket},
    hitl, ApprovalRef, HitlDecisionOutcome, NatsSession, PromptProgress,
    CONTROL_ACK_ATTEMPT_TIMEOUT, CONTROL_ACK_RETRY_DELAY, CONTROL_RECOVERY_TIMEOUT,
};
use crate::nats_worker::{request_control_command, ControlCommand};
use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;
use outcome::{is_matching_pending, receipt_outcome};

pub(super) struct HitlDecisionTarget<'a> {
    pub reference: &'a ApprovalRef,
    pub approved: bool,
    pub prompt_seq: u64,
    pub prompt_id: &'a str,
}

struct HitlApprovalIntent<'a> {
    pub tool_call_id: &'a str,
    pub approved: bool,
    pub note: Option<String>,
}

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
        let target = HitlDecisionTarget {
            reference,
            approved,
            prompt_seq,
            prompt_id: &prompt_id,
        };
        if let Some(outcome) = self.initial_hitl_decision_state(&target, &entries).await? {
            return Ok(outcome);
        }
        self.dispatch_and_await_hitl_decision(&target, note).await
    }

    async fn initial_hitl_decision_state(
        &self,
        target: &HitlDecisionTarget<'_>,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<HitlDecisionOutcome>> {
        let Some(outcome) = self.hitl_decision_state(target, entries)? else {
            return Ok(None);
        };
        if matches!(outcome, HitlDecisionOutcome::AlreadyApplied { .. }) {
            self.publish_activation(target.prompt_seq, None, None)
                .await?;
        }
        Ok(Some(outcome))
    }

    async fn dispatch_and_await_hitl_decision(
        &self,
        target: &HitlDecisionTarget<'_>,
        note: Option<String>,
    ) -> Result<HitlDecisionOutcome> {
        self.publish_activation(target.prompt_seq, None, None)
            .await?;
        let command = ControlCommand::HitlApprovalDecision {
            tool_call_id: target.reference.tool_call_id.clone(),
            approved: target.approved,
            note,
            request_seq: Some(target.reference.request_seq),
        };
        let request = hitl::ApprovalRequest::from(target.reference);
        let deadline = tokio::time::Instant::now() + CONTROL_RECOVERY_TIMEOUT;
        loop {
            if let Some(outcome) = self
                .poll_hitl_decision_attempt(target, &request, &command)
                .await?
            {
                return Ok(outcome);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for worker to apply HITL approval '{}'",
                    target.reference.tool_call_id
                );
            }
            tokio::time::sleep(CONTROL_ACK_RETRY_DELAY).await;
        }
    }

    async fn poll_hitl_decision_attempt(
        &self,
        target: &HitlDecisionTarget<'_>,
        request: &hitl::ApprovalRequest<'_>,
        command: &ControlCommand,
    ) -> Result<Option<HitlDecisionOutcome>> {
        let _ = request_control_command(
            &self.client,
            &self.storage_key,
            command,
            CONTROL_ACK_ATTEMPT_TIMEOUT,
        )
        .await;
        let entries = self.load_durable_entries().await?;
        if let Some((decided, _)) = request.decision_with_note(&entries)? {
            return Ok(Some(if decided == target.approved {
                HitlDecisionOutcome::Applied
            } else {
                HitlDecisionOutcome::Conflicting
            }));
        }
        self.hitl_decision_state(target, &entries)
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
        let intent = HitlApprovalIntent {
            tool_call_id,
            approved,
            note,
        };
        let entries = self.load_durable_entries().await?;
        let pending = crate::nats_worker::derive_pending_hitl_approvals(&entries)?;
        if let Some(pending) = pending.into_iter().find(|i| i.tool_call_id == tool_call_id) {
            self.decide_pending_approval(intent, pending, &entries)
                .await
        } else {
            self.decide_already_resolved_approval(intent, &entries)
                .await
        }
    }

    async fn decide_pending_approval(
        &self,
        intent: HitlApprovalIntent<'_>,
        pending: super::PendingHitlApproval,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<bool> {
        let request = hitl::ApprovalRequest {
            tool_call_id: intent.tool_call_id,
            tool_round_seq: pending.tool_round_seq,
            request_seq: pending.request_seq,
        };
        let reference = self
            .hitl_reference(&request, entries)
            .await?
            .context("pending HITL approval has no invocation admission")?;
        let outcome = self
            .decide_hitl_approval_for(&reference, intent.approved, intent.note)
            .await?;
        if is_applied_outcome(&outcome) {
            return Ok(true);
        }
        let entries = self.load_durable_entries().await?;
        Ok(request.outcome(&entries, intent.approved)? == Some(true))
    }

    async fn decide_already_resolved_approval(
        &self,
        intent: HitlApprovalIntent<'_>,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<bool> {
        let decided = hitl::already_decided(entries, intent.tool_call_id, intent.approved)?;
        if !decided {
            return Ok(false);
        }
        if let Some(request) = hitl::latest_request(entries, intent.tool_call_id)? {
            if let Some(reference) = self.hitl_reference(&request, entries).await? {
                let _ = self
                    .decide_hitl_approval_for(&reference, intent.approved, intent.note)
                    .await?;
            }
        }
        Ok(true)
    }

    /// None means still pending. Every resolved state is scoped to the root and
    /// latest approval, rather than merely the tool ID or physical request tail.
    fn hitl_decision_state(
        &self,
        target: &HitlDecisionTarget<'_>,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<HitlDecisionOutcome>> {
        let scope = hitl::InvocationProgressScope {
            invocation_id: &target.reference.invocation_id,
            prompt_id: target.prompt_id,
            prompt_sequence: target.prompt_seq,
            session_id: &self.session_id,
        };
        let progress = Self::invocation_progress_from_entries(&scope, entries)?;
        match progress {
            PromptProgress::InputRequired(pending) if is_matching_pending(&pending, target) => {
                Ok(None)
            }
            PromptProgress::Running {
                last_decision: Some(receipt),
            } if receipt.request == *target.reference => {
                Ok(Some(receipt_outcome(receipt, target.approved)))
            }
            _ => Ok(Some(HitlDecisionOutcome::Stale)),
        }
    }

    fn fixed_ticket_root(
        storage_key: &str,
        reference: &ApprovalRef,
        ticket: &FixedAdmissionTicket,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<(u64, String)>> {
        if ticket.storage_key() != storage_key || ticket.invocation_id() != reference.invocation_id
        {
            return Ok(None);
        }
        let FixedAdmissionOutcome::Admitted { prompt_sequence } = ticket.outcome(entries)? else {
            return Ok(None);
        };
        Ok(Some((prompt_sequence, ticket.prompt_id().to_owned())))
    }

    async fn round_prompt_root(
        &self,
        reference: &ApprovalRef,
        entries: &[(u64, SessionLogEntry)],
    ) -> Result<Option<(u64, String)>> {
        let Some((seq, prompt_id, invocation_id)) = self
            .hitl_round_prompt(reference.tool_round_seq, entries)
            .await?
        else {
            return Ok(None);
        };
        if invocation_id != reference.invocation_id {
            return Ok(None);
        }
        Ok(Some((seq, prompt_id)))
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
        let root = match admission.fixed_ticket {
            Some(ticket) => {
                Self::fixed_ticket_root(&self.storage_key, reference, &ticket, entries)?
            }
            None => self.round_prompt_root(reference, entries).await?,
        };
        let Some((seq, prompt_id)) = root else {
            return Ok(None);
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
            let Some(id) = user_message_id(entry) else {
                continue;
            };
            if let Some(admission) = self
                .metadata_store
                .prompt_admission(&self.storage_key, id)
                .await?
            {
                return Ok(Some((
                    *seq,
                    id.to_owned(),
                    admission.invocation_id.as_str().into(),
                )));
            }
        }
        Ok(None)
    }
}

fn user_message_id(entry: &SessionLogEntry) -> Option<&str> {
    match entry {
        SessionLogEntry::Message {
            id: Some(id), role, ..
        } if role.is_user() => Some(id),
        _ => None,
    }
}

fn is_applied_outcome(outcome: &HitlDecisionOutcome) -> bool {
    matches!(
        outcome,
        HitlDecisionOutcome::Applied | HitlDecisionOutcome::AlreadyApplied { .. }
    )
}
