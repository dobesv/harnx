//! Resolve middleware challenges by the receipt's exact log position, never tool ID alone.
use anyhow::{ensure, Context, Result};
use harnx_core::{
    interaction::{InteractionRequest, InteractionResponse},
    session::SessionLogEntry,
    session_reconstruct::apply_log_mutations_nats,
};
use harnx_runtime::{
    interactions::{derive_pending_interactions, InteractionDecisionOutcome},
    nats_session_log::NatsSessionLog,
    ApprovalRef, HitlDecisionOutcome, HitlDecisionReceipt, NatsSession, PendingHitlApproval,
};

mod progress;
pub(crate) use progress::fixed_progress;

async fn entries(session: &NatsSession) -> Result<Vec<(u64, SessionLogEntry)>> {
    let metadata = session.metadata_store();
    let entries = NatsSessionLog::new_with_replicas(
        metadata.jetstream().clone(),
        session.storage_key(),
        metadata.replicas(),
    )
    .load_events_latest_required_async()
    .await?;
    apply_log_mutations_nats(&entries)
}

fn request_at(
    entries: &[(u64, SessionLogEntry)],
    target: &ApprovalRef,
) -> Option<InteractionRequest> {
    entries.iter().find_map(|(seq, entry)| match entry {
        SessionLogEntry::InteractionRequested {
            tool_round_seq,
            tool_call_id,
            ..
        } if *seq == target.request_seq
            && *tool_round_seq == target.tool_round_seq
            && *tool_call_id == target.tool_call_id =>
        {
            harnx_runtime::interactions::interaction_request_from_entry(entry)
        }
        _ => None,
    })
}

pub(crate) async fn render_challenge(
    session: &NatsSession,
    pending: &PendingHitlApproval,
) -> Result<String> {
    let Some(id) = &pending.interaction_id else {
        return Ok(super::render_challenge(pending));
    };
    let pending_interactions = derive_pending_interactions(&entries(session).await?)?;
    let interaction = pending_interactions
        .iter()
        .find(|item| item.interaction_id == *id && item.request_seq == pending.request_seq)
        .context("interaction is no longer pending")?;
    let instructions = if interaction.request.kind == "confirm" {
        "Reply `yes` to approve or `no` to deny. An optional note can follow the decision."
    } else {
        "Interaction kind not supported by this client. Reply `no` to deny; approval is not supported."
    };
    Ok(format!("{}\n\n{instructions}", interaction.summary()))
}

pub(crate) async fn can_approve(session: &NatsSession, target: &ApprovalRef) -> Result<bool> {
    Ok(request_at(&entries(session).await?, target).is_none_or(|r| r.kind == "confirm"))
}

/// The saved request sequence uniquely names its immutable interaction ID. Keeping
/// that lookup here preserves old checkpoint/receipt documents without a schema change.
pub(crate) async fn decide(
    session: &NatsSession,
    target: &ApprovalRef,
    approved: bool,
    note: Option<String>,
) -> Result<HitlDecisionOutcome> {
    let Some(request) = request_at(&entries(session).await?, target) else {
        return session
            .decide_hitl_approval_for(target, approved, note)
            .await;
    };
    let response = response_for(&request, approved, note.clone())?;
    let outcome = session
        .decide_interaction(&request.interaction_id, target.request_seq, &response)
        .await?;
    decision_outcome(outcome, approved, note)
}

fn response_for(
    request: &InteractionRequest,
    approved: bool,
    note: Option<String>,
) -> Result<InteractionResponse> {
    ensure!(
        !approved || request.kind == "confirm",
        "interaction kind not supported by this client"
    );
    Ok(InteractionResponse {
        action: if approved { "approve" } else { "deny" }.into(),
        data: Default::default(),
        note,
    })
}

fn decision_outcome(
    outcome: InteractionDecisionOutcome,
    approved: bool,
    note: Option<String>,
) -> Result<HitlDecisionOutcome> {
    Ok(match outcome {
        InteractionDecisionOutcome::Applied => HitlDecisionOutcome::Applied,
        InteractionDecisionOutcome::AlreadyApplied => {
            HitlDecisionOutcome::AlreadyApplied { approved, note }
        }
        InteractionDecisionOutcome::Conflicting => HitlDecisionOutcome::Conflicting,
        InteractionDecisionOutcome::WrongSequence
        | InteractionDecisionOutcome::Stale
        | InteractionDecisionOutcome::AfterComplete => HitlDecisionOutcome::Stale,
        InteractionDecisionOutcome::LeaseLost => {
            anyhow::bail!("interaction decision lease lost; retry the same reply")
        }
    })
}

/// Middleware decisions don't populate runtime's legacy last_decision field.
/// Recover only this checkpoint's accepted response, including cross-frontend answers.
pub(crate) async fn accepted_decision(
    session: &NatsSession,
    target: &ApprovalRef,
) -> Result<Option<HitlDecisionReceipt>> {
    let entries = entries(session).await?;
    let Some(request) = request_at(&entries, target) else {
        return Ok(None);
    };
    Ok(entries.iter().find_map(|(seq, entry)| match entry {
        SessionLogEntry::InteractionResponse {
            request_seq,
            interaction_id,
            tool_call_id,
            response,
            ..
        } if *request_seq == target.request_seq
            && *interaction_id == request.interaction_id
            && *tool_call_id == target.tool_call_id =>
        {
            Some(HitlDecisionReceipt {
                request: target.clone(),
                decision_seq: *seq,
                approved: response.action == "approve",
                note: response.note.clone(),
            })
        }
        _ => None,
    }))
}

/// Concurrent calls can persist requests in a different order from the manifest.
/// A replacement must be the first current item, with the old item no longer pending.
pub(crate) async fn is_manifest_successor(
    session: &NatsSession,
    previous: &ApprovalRef,
    next: &ApprovalRef,
) -> Result<bool> {
    let entries = entries(session).await?;
    let Some(old) = request_at(&entries, previous) else {
        return Ok(false);
    };
    let pending = derive_pending_interactions(&entries)?;
    Ok(previous.invocation_id == next.invocation_id
        && !pending
            .iter()
            .any(|p| p.interaction_id == old.interaction_id)
        && pending.first().is_some_and(|p| {
            p.request_seq == next.request_seq
                && p.tool_round_seq == next.tool_round_seq
                && p.tool_call_id == next.tool_call_id
        }))
}
