//! Durable interaction response acceptance.

use anyhow::Result;
use harnx_core::{
    interaction::InteractionResponse, session::SessionLogEntry,
    session_reconstruct::apply_log_mutations_nats,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::pending::{
    entries_for_round, existing_response, find_request_by_id, latest_suspension_ready,
    round_is_terminal, PendingInteraction,
};
use crate::nats_lease::NatsSessionLease;
use crate::nats_worker::{FencedSessionLogSink, NatsSessionLogBackend};

/// Result of evaluating and applying an interaction response against durable session state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum InteractionDecisionOutcome {
    /// Request was pending; the response was appended with lease fence.
    Applied,
    /// Identical response was already durable. Acknowledged without second append.
    AlreadyApplied,
    /// Conflicting response was already accepted for this request.
    Conflicting,
    /// The request sequence does not match the requested interaction.
    WrongSequence,
    /// Interaction is not currently pending (unready, not in latest SuspensionReady, or unknown).
    Stale,
    /// The round was already completed with ToolResults or cancelled/cleared.
    AfterComplete,
    /// Response failed validation against the requested surface (`a2ui`
    /// only): undeclared action, or `data` keys/types inconsistent with the
    /// surface's bound inputs and action context. Nothing was appended.
    InvalidResponse,
    /// The lease is not held or fence CAS failed.
    LeaseLost,
}

impl InteractionDecisionOutcome {
    /// True when the response was accepted (either newly applied or idempotent retry).
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied | Self::AlreadyApplied)
    }
}

/// Accept an interaction response with lease-fenced CAS.
///
/// Enforces:
/// - Lease must be held and revalidated against broker
/// - Round must not be completed (`ToolResults`) or cancelled
/// - Request sequence must match the `InteractionRequested` entry exactly
/// - Identical retry is acknowledged without append (`AlreadyApplied`) even across manifest changes
/// - Unanswered request must be listed in latest `SuspensionReady.pending` manifest for the round
/// - `a2ui` responses are validated against the requested surface (declared
///   action, data shape): a violation is `InvalidResponse` and is never
///   appended. `confirm` and other kinds keep their existing acceptance.
/// - Conflicting, stale, mismatched replies are rejected
pub async fn accept_interaction_response(
    backend: &NatsSessionLogBackend,
    lease: &Arc<NatsSessionLease>,
    interaction_id: &str,
    request_seq: u64,
    response: &InteractionResponse,
) -> Result<InteractionDecisionOutcome> {
    if !lease.is_held() || !lease.revalidate_ownership().await.unwrap_or(false) {
        return Ok(InteractionDecisionOutcome::LeaseLost);
    }
    let entries: Vec<(u64, SessionLogEntry)> = backend.load_events_latest_async().await?;
    let effective = apply_log_mutations_nats(&entries)?;

    let Some(requested) = find_request_by_id(&effective, interaction_id) else {
        return Ok(InteractionDecisionOutcome::Stale);
    };
    if requested.request_seq != request_seq {
        return Ok(InteractionDecisionOutcome::WrongSequence);
    }

    let round_seq = requested.tool_round_seq;
    if round_is_terminal(&effective, round_seq) {
        return Ok(InteractionDecisionOutcome::AfterComplete);
    }

    let round_entries = entries_for_round(&effective, round_seq);

    // Existing response checked first: idempotent identical retries remain AlreadyApplied
    // even if a subsequent manifest changed or reprompted, until round is complete.
    if let Some(existing) = existing_response(round_entries, interaction_id, request_seq) {
        return Ok(if existing == *response {
            InteractionDecisionOutcome::AlreadyApplied
        } else {
            InteractionDecisionOutcome::Conflicting
        });
    }

    // `a2ui` responses are validated against the requested surface before
    // the manifest check, so nothing invalid is ever appended.
    if let Err(reason) = validate_surface_response(&requested.request, response) {
        log::warn!(
            "rejected interaction response for '{}': {reason}",
            requested.interaction_id
        );
        return Ok(InteractionDecisionOutcome::InvalidResponse);
    }

    // Unanswered request must be present in latest SuspensionReady manifest
    let Some(manifest) = latest_suspension_ready(round_entries, round_seq) else {
        return Ok(InteractionDecisionOutcome::Stale);
    };
    if !manifest.contains(&interaction_id.to_string()) {
        return Ok(InteractionDecisionOutcome::Stale);
    }

    commit_response_cas(
        backend,
        lease,
        requested,
        response,
        entries.last().map_or(0, |(s, _)| *s),
    )
    .await
}

/// Alias for `accept_interaction_response`.
pub use accept_interaction_response as apply_interaction_response;

/// The one surface check every frontend's `a2ui` answer passes through. Other
/// kinds (`confirm`, and the pre-T4.2 kinds) keep their existing acceptance.
fn validate_surface_response(
    request: &harnx_core::interaction::InteractionRequest,
    response: &InteractionResponse,
) -> Result<(), String> {
    if request.kind != "a2ui" {
        return Ok(());
    }
    harnx_core::interaction::response::validate_response(&request.payload, response)
}

async fn commit_response_cas(
    backend: &NatsSessionLogBackend,
    lease: &Arc<NatsSessionLease>,
    requested: PendingInteraction,
    response: &InteractionResponse,
    expected_tail: u64,
) -> Result<InteractionDecisionOutcome> {
    let round_seq = requested.tool_round_seq;
    let interaction_id = requested.interaction_id;
    let request_seq = requested.request_seq;
    let entry = SessionLogEntry::InteractionResponse {
        request_seq,
        interaction_id: interaction_id.clone(),
        tool_call_id: requested.tool_call_id,
        response: response.clone(),
        fence_token: lease.fence_token(),
    };
    let sink = FencedSessionLogSink::new(backend.clone(), Arc::clone(lease));
    crate::fault_injection::hit(
        backend.session_id(),
        crate::fault_injection::RecoveryFaultPoint::AcceptedResponseBefore,
    )
    .await;
    match sink.append_hitl_event_cas(&entry, expected_tail).await {
        Ok(Some(_)) => {
            crate::fault_injection::hit(
                backend.session_id(),
                crate::fault_injection::RecoveryFaultPoint::AcceptedResponseAfter,
            )
            .await;
            Ok(InteractionDecisionOutcome::Applied)
        }
        Ok(None) => {
            resolve_cas_loss(backend, round_seq, &interaction_id, request_seq, response).await
        }
        Err(err) => Err(err),
    }
}

async fn resolve_cas_loss(
    backend: &NatsSessionLogBackend,
    round_seq: u64,
    interaction_id: &str,
    request_seq: u64,
    response: &InteractionResponse,
) -> Result<InteractionDecisionOutcome> {
    let reloaded: Vec<(u64, SessionLogEntry)> = backend.load_events_latest_async().await?;
    let effective = apply_log_mutations_nats(&reloaded)?;
    let round_entries = entries_for_round(&effective, round_seq);
    if let Some(existing) = existing_response(round_entries, interaction_id, request_seq) {
        Ok(if existing == *response {
            InteractionDecisionOutcome::AlreadyApplied
        } else {
            InteractionDecisionOutcome::Conflicting
        })
    } else {
        Ok(InteractionDecisionOutcome::LeaseLost)
    }
}
