//! Durable runtime ordering decides completion versus exact interruption.
use super::*;
use harnx_runtime::nats_session::fixed_admission::{FixedAdmissionOutcome, FixedAdmissionTicket};

type RecoveryOutcome = (TaskState, Option<String>, AdmissionPhase, Option<u64>);
const OWNER_INTERRUPTED: &str = "interrupted by server restart";

pub(super) fn recovery_status(
    state: TaskState,
    text: Option<&str>,
    canceled: bool,
) -> a2a_lf::TaskStatus {
    if state != TaskState::Failed {
        return status(state, text);
    }
    if text != Some(OWNER_INTERRUPTED) {
        return status(state, text);
    }
    if canceled {
        status(TaskState::Canceled, None)
    } else {
        status(state, text)
    }
}

fn interrupted_outcome(
    canceled: bool,
    phase: AdmissionPhase,
    prompt: Option<u64>,
) -> RecoveryOutcome {
    let state = if canceled {
        TaskState::Canceled
    } else {
        TaskState::Failed
    };
    let text = if canceled {
        None
    } else {
        Some(OWNER_INTERRUPTED.into())
    };
    (state, text, phase, prompt)
}

fn completed_outcome(
    result: harnx_runtime::NatsTurnResult,
    canceled: bool,
    output_limit: usize,
) -> RecoveryOutcome {
    let seq = result.user_msg_seq;
    if result.was_cancelled {
        return interrupted_outcome(canceled, AdmissionPhase::Admitted, Some(seq));
    }
    let (state, text) =
        crate::runner::limits::bounded_outcome(turn_outcome(result, ""), output_limit);
    (state, text, AdmissionPhase::Admitted, Some(seq))
}

pub(super) async fn recovery_outcome(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
    canceled: bool,
    output_limit: usize,
) -> Result<RecoveryOutcome> {
    // The log orders TurnEnd against the exact fenced Cancel append.
    if let Some(result) = session.fixed_prompt_completion(ticket).await? {
        return Ok(completed_outcome(result, canceled, output_limit));
    }
    match session.close_fixed_admission(ticket).await? {
        FixedAdmissionOutcome::Admitted { prompt_sequence } => {
            session
                .interrupt_prompt(prompt_sequence, "A2A admission owner lost")
                .await?;
            let result = session
                .fixed_prompt_completion(ticket)
                .await?
                .context("scoped stop unconfirmed")?;
            Ok(completed_outcome(result, canceled, output_limit))
        }
        FixedAdmissionOutcome::Closed { .. } | FixedAdmissionOutcome::Fenced { .. } => {
            Ok(interrupted_outcome(canceled, AdmissionPhase::Closed, None))
        }
        FixedAdmissionOutcome::Pending => anyhow::bail!("admission closure unconfirmed"),
    }
}
