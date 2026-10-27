//! Exact pending-request checks and lease-fenced decision append.
use super::*;
use crate::PendingHitlApproval;

impl SessionControlHandler {
    pub(super) async fn apply_hitl_decision(
        &self,
        params: HitlDecisionParams,
        reply: Option<async_nats::Subject>,
    ) {
        if !should_append_control_log_entry(&self.lease) {
            return;
        }
        let Some(entries) = self.load_entries_for_decision().await else {
            return;
        };
        let Some(pending) = derive_pending_for_decision(&entries) else {
            return;
        };
        if !is_eligible_hitl_decision(&pending, &entries, &params) {
            return;
        }
        if !self.lease.is_held() {
            return;
        }
        if !self.commit_hitl_decision(&entries, &params).await {
            return;
        }
        self.publish_updated().await;
        let _ = self.hitl_decision_tx.send(AppliedHitlDecision {
            tool_call_id: params.tool_call_id,
            approved: params.approved,
            note: params.note,
        });
        self.acknowledge(reply).await;
    }

    async fn load_entries_for_decision(
        &self,
    ) -> Option<Vec<(u64, harnx_core::session::SessionLogEntry)>> {
        match self.backend.load_events_latest_async().await {
            Ok(entries) => Some(entries),
            Err(error) => {
                log::warn!("failed to load pending HITL approvals: {error:#}");
                None
            }
        }
    }

    async fn commit_hitl_decision(
        &self,
        entries: &[(u64, harnx_core::session::SessionLogEntry)],
        params: &HitlDecisionParams,
    ) -> bool {
        let entry = harnx_core::session::SessionLogEntry::HitlApprovalDecision {
            tool_call_id: params.tool_call_id.clone(),
            approved: params.approved,
            note: params.note.clone(),
            fence_token: self.lease.fence_token(),
        };
        let expected_last_sequence = entries.last().map_or(0, |(seq, _)| *seq);
        let sink = crate::nats_worker::backend::FencedSessionLogSink::new(
            self.backend.clone(),
            Arc::clone(&self.lease),
        );
        match sink
            .append_hitl_event_cas(&entry, expected_last_sequence)
            .await
        {
            Ok(Some(_)) => true,
            Ok(None) => {
                self.handle_decision_cas_loss().await;
                false
            }
            Err(error) => {
                log::warn!("failed to append HITL approval decision: {error:#}");
                false
            }
        }
    }

    async fn handle_decision_cas_loss(&self) {
        match self.backend.load_events_latest_async().await {
            Ok(reloaded) => {
                if let Err(error) =
                    crate::nats_worker::agent_loop::derive_pending_hitl_approvals(&reloaded)
                {
                    log::warn!(
                        "failed to re-derive HITL approvals after decision CAS loss: {error:#}"
                    );
                }
            }
            Err(error) => {
                log::warn!("failed to reload HITL approvals after decision CAS loss: {error:#}");
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct HitlDecisionParams {
    pub tool_call_id: String,
    pub approved: bool,
    pub note: Option<String>,
    pub request_seq: Option<u64>,
}

impl HitlDecisionParams {
    pub fn new(
        tool_call_id: impl Into<String>,
        approved: bool,
        note: Option<String>,
        request_seq: Option<u64>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            approved,
            note,
            request_seq,
        }
    }
}

fn derive_pending_for_decision(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
) -> Option<Vec<PendingHitlApproval>> {
    match crate::nats_worker::agent_loop::derive_pending_hitl_approvals(entries) {
        Ok(pending) => Some(pending),
        Err(error) => {
            log::warn!("failed to derive pending HITL approvals: {error:#}");
            None
        }
    }
}

fn is_eligible_hitl_decision(
    pending: &[PendingHitlApproval],
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    params: &HitlDecisionParams,
) -> bool {
    pending.iter().any(|approval| {
        matches_requested_approval(approval, params)
            && !has_terminal_entry_after(entries, approval.tool_round_seq)
    })
}

fn has_terminal_entry_after(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    tool_round_seq: u64,
) -> bool {
    entries.iter().any(|(seq, entry)| {
        *seq > tool_round_seq
            && matches!(
                entry,
                harnx_core::session::SessionLogEntry::TurnEnd { .. }
                    | harnx_core::session::SessionLogEntry::Cancel { .. }
                    | harnx_core::session::SessionLogEntry::Error { .. }
            )
    })
}

fn matches_requested_approval(approval: &PendingHitlApproval, params: &HitlDecisionParams) -> bool {
    if approval.tool_call_id != params.tool_call_id {
        return false;
    }
    params
        .request_seq
        .is_none_or(|expected| expected == approval.request_seq)
}
