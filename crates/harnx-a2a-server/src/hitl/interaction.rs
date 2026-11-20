//! Resolve middleware challenges by the receipt's exact log position, never tool ID alone.
use a2a_lf::{Message, Part, Role, TaskState, TaskStatus};
use anyhow::{Context, Result};
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

mod a2ui;
pub(crate) use a2ui::InvalidReply;
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

pub(crate) async fn challenge_status(
    session: &NatsSession,
    pending: &PendingHitlApproval,
) -> Result<TaskStatus> {
    let mut parts = vec![Part::text(super::render_challenge(pending))];
    if let Some(id) = &pending.interaction_id {
        let pending_interactions = derive_pending_interactions(&entries(session).await?)?;
        let interaction = pending_interactions
            .first()
            .filter(|item| item.interaction_id == *id && item.request_seq == pending.request_seq)
            .context("interaction is no longer the pending head")?;
        parts = challenge_parts(&interaction.request);
    }
    Ok(TaskStatus {
        state: TaskState::InputRequired,
        message: Some(Message::new(Role::Agent, parts)),
        timestamp: Some(chrono::Utc::now()),
    })
}

fn challenge_parts(request: &InteractionRequest) -> Vec<Part> {
    if request.kind == "a2ui" {
        return vec![
            Part::text(harnx_core::interaction::text::render(&request.payload)),
            Part::data(request.payload.clone()).with_media_type(a2ui::MIME),
        ];
    }
    let instructions = if request.kind == "confirm" {
        "Reply `yes` to approve or `no` to deny. An optional note can follow the decision."
    } else {
        "Interaction kind not supported by this client. Reply `no` to deny; approval is not supported."
    };
    vec![Part::text(format!("{}\n\n{instructions}", request.summary))]
}

pub(crate) fn correction(status: &mut TaskStatus, reason: &str) {
    status.state = TaskState::InputRequired;
    status.message.iter_mut().for_each(|message| {
        let prefix = correction_prefix(&message.parts, reason);
        prepend_correction(message, &prefix);
    });
}

fn correction_prefix(parts: &[Part], reason: &str) -> String {
    if parts
        .iter()
        .any(|part| part.media_type.as_deref() == Some(a2ui::MIME))
    {
        format!("Reply rejected: {reason}. Correct the answer and send a new messageId.\n\n")
    } else {
        "Reply not recognized or not supported by this client.\n\n".into()
    }
}

fn prepend_correction(message: &mut Message, prefix: &str) {
    if let Some(Part {
        content: a2a_lf::PartContent::Text(text),
        ..
    }) = message.parts.first_mut()
    {
        text.insert_str(0, prefix);
    }
}

pub(crate) async fn parse_response(
    session: &NatsSession,
    target: &ApprovalRef,
    parts: &[Part],
) -> Result<InteractionResponse> {
    let request = request_at(&entries(session).await?, target);
    response_for(request.as_ref(), parts)
}

fn response_for(
    request: Option<&InteractionRequest>,
    parts: &[Part],
) -> Result<InteractionResponse> {
    if let Some(request) = request.filter(|r| r.kind == "a2ui") {
        return a2ui::parse(&request.payload, parts);
    }
    let (approved, note) = match super::parse_reply(parts) {
        super::ReplyParse::Approve(note) => (true, note),
        super::ReplyParse::Reject(note) => (false, note),
        super::ReplyParse::Unrecognized => {
            return Err(InvalidReply("reply not recognized as approval or rejection".into()).into())
        }
    };
    if approved && request.is_some_and(|r| r.kind != "confirm") {
        return Err(InvalidReply("interaction kind not supported by this client".into()).into());
    }
    Ok(InteractionResponse {
        action: if approved { "approve" } else { "deny" }.into(),
        data: Default::default(),
        note,
    })
}

/// Resolve only the immutable request bound to this receipt, including recovery.
pub(crate) async fn decide(
    session: &NatsSession,
    target: &ApprovalRef,
    parts: &[Part],
) -> Result<HitlDecisionOutcome> {
    let request = request_at(&entries(session).await?, target);
    let response = response_for(request.as_ref(), parts)?;
    deliver_response(session, target, request, response).await
}

async fn deliver_response(
    session: &NatsSession,
    target: &ApprovalRef,
    request: Option<InteractionRequest>,
    response: InteractionResponse,
) -> Result<HitlDecisionOutcome> {
    match request {
        Some(request) => deliver_interaction(session, target, &request, &response).await,
        None => {
            session
                .decide_hitl_approval_for(target, approves(&response), response.note)
                .await
        }
    }
}

fn approves(response: &InteractionResponse) -> bool {
    response.action != "deny"
}

async fn deliver_interaction(
    session: &NatsSession,
    target: &ApprovalRef,
    request: &InteractionRequest,
    response: &InteractionResponse,
) -> Result<HitlDecisionOutcome> {
    let outcome = session
        .decide_interaction(&request.interaction_id, target.request_seq, response)
        .await?;
    decision_outcome(outcome, response.action != "deny", response.note.clone())
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
        InteractionDecisionOutcome::InvalidResponse => {
            return Err(InvalidReply(
                "undeclared action or invalid data (check input types and option values)".into(),
            )
            .into());
        }
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
                approved: response.action != "deny",
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
