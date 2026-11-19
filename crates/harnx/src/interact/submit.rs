//! Submitting one interaction answer over the durable acceptance path and
//! reporting the worker's outcome.

use anyhow::{Context, Result};
use harnx_core::interaction::InteractionResponse;
use harnx_runtime::{interactions::PendingInteraction, NatsSession};

/// The CLI message for one durable acceptance outcome. The bool is the exit
/// code's success flag; outcomes are never a process failure.
fn outcome_message(
    outcome: harnx_runtime::interactions::InteractionDecisionOutcome,
    id: &str,
) -> (bool, String) {
    use harnx_runtime::interactions::InteractionDecisionOutcome;
    match outcome {
        InteractionDecisionOutcome::Applied => (
            true,
            format!("Accepted: recorded the response for interaction '{id}'."),
        ),
        InteractionDecisionOutcome::AlreadyApplied => (
            true,
            format!("Already applied: an identical response for '{id}' was recorded earlier."),
        ),
        InteractionDecisionOutcome::Conflicting => (
            false,
            format!(
                "Rejected: a different response for '{id}' was already accepted \
                 (conflicting). Nothing was recorded."
            ),
        ),
        InteractionDecisionOutcome::WrongSequence => (
            false,
            format!("Rejected: the request sequence does not match '{id}' (wrong_sequence)."),
        ),
        InteractionDecisionOutcome::Stale => (
            false,
            format!(
                "Rejected: interaction '{id}' is no longer pending (stale). Nothing was recorded."
            ),
        ),
        InteractionDecisionOutcome::AfterComplete => (
            false,
            format!(
                "Rejected: the round that asked '{id}' already completed or was \
                 cancelled (after_complete)."
            ),
        ),
        InteractionDecisionOutcome::InvalidResponse => (
            false,
            "Rejected: the response does not match the requested surface \
             (invalid_response). Nothing was recorded."
                .to_string(),
        ),
        InteractionDecisionOutcome::LeaseLost => (
            false,
            "Rejected: the worker lost its session lease before recording \
             (lease_lost); retry."
                .to_string(),
        ),
    }
}

/// Print the outcome message and map it to the exit code.
fn report_outcome(accepted: bool, message: &str) -> Result<u8> {
    if accepted {
        println!("{message}");
        Ok(0)
    } else {
        eprintln!("{message}");
        Ok(1)
    }
}

/// Attach the session the interaction belongs to.
async fn attach_session(
    config: crate::config::Config,
    args: &crate::cli::InteractAnswerArgs,
) -> Result<NatsSession> {
    crate::session_attach::attach_session(config, &args.agent, &args.session).await
}

/// Submit the planned response through the durable acceptance path
/// (`NatsSession::decide_interaction`) and report the outcome.
pub(super) async fn submit_answer(
    config: crate::config::Config,
    args: &crate::cli::InteractAnswerArgs,
    target: PendingInteraction,
    response: InteractionResponse,
) -> Result<u8> {
    let session = attach_session(config, args).await?;
    let outcome = session
        .decide_interaction(&target.interaction_id, target.request_seq, &response)
        .await
        .context("submitting the interaction response failed")?;
    let (accepted, message) = outcome_message(outcome, &target.interaction_id);
    report_outcome(accepted, &message)
}
