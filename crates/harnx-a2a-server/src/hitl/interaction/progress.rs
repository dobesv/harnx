//! A2A understands ready middleware requests that the mixed-version runtime guard parks.
use anyhow::Result;
use harnx_runtime::{
    interactions::derive_pending_interactions,
    nats_session::fixed_admission::{FixedAdmissionOutcome, FixedAdmissionTicket},
    NatsSession, PendingHitlApproval, PromptProgress,
};

pub(crate) async fn fixed_progress(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
) -> Result<PromptProgress> {
    let progress = session.fixed_prompt_progress(ticket).await?;
    if !matches!(progress, PromptProgress::Parked) {
        return Ok(progress);
    }
    let FixedAdmissionOutcome::Admitted { prompt_sequence } =
        session.resolve_fixed_admission(ticket).await?
    else {
        return Ok(PromptProgress::Parked);
    };
    let pending = derive_pending_interactions(&super::entries(session).await?)?
        .into_iter()
        .find(|p| p.tool_round_seq > prompt_sequence);
    let Some(pending) = pending else {
        return Ok(PromptProgress::Parked);
    };
    // A decision/termination can race the second log read. Runtime's terminal
    // proof still wins; decide_interaction fences any stale nonterminal answer.
    if let Some(terminal) = session.fixed_prompt_completion(ticket).await? {
        return Ok(PromptProgress::Terminal(terminal));
    }
    Ok(PromptProgress::InputRequired(PendingHitlApproval {
        request_seq: pending.request_seq,
        tool_round_seq: pending.tool_round_seq,
        tool_call_id: pending.tool_call_id,
        tool_name: pending.request.producer.clone(),
        arguments: pending.request.payload.clone(),
        summary: pending.request.summary,
        interaction_id: Some(pending.interaction_id),
    }))
}
