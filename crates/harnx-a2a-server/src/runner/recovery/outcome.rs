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
        // Runtime deadlines cancel without an A2A CancelIntent. Ordinary owner
        // loss still fails, even though its scoped stop also writes Cancel.
        return interrupted_outcome(
            canceled || crate::runner::deadline_cancelled(&result),
            AdmissionPhase::Admitted,
            Some(seq),
        );
    }
    let (state, text) =
        crate::runner::limits::bounded_outcome(turn_outcome(result, ""), output_limit);
    (state, text, AdmissionPhase::Admitted, Some(seq))
}

struct RecoveryContext<'a> {
    session: &'a NatsSession,
    ticket: &'a FixedAdmissionTicket,
    output_limit: usize,
}

async fn handle_admitted_closure(
    cx: &RecoveryContext<'_>,
    prompt_sequence: u64,
    canceled: bool,
) -> Result<RecoveryOutcome> {
    if !canceled {
        match cx.session.fixed_prompt_progress(cx.ticket).await? {
            harnx_runtime::PromptProgress::Terminal(result) => {
                return Ok(completed_outcome(result, false, cx.output_limit));
            }
            harnx_runtime::PromptProgress::Running {
                last_decision: None,
            } => {}
            _ => anyhow::bail!("HITL continuation requires progress recovery"),
        }
    }
    cx.session
        .interrupt_prompt(prompt_sequence, "A2A admission owner lost")
        .await?;
    let result = cx
        .session
        .fixed_prompt_completion(cx.ticket)
        .await?
        .context("scoped stop unconfirmed")?;
    Ok(completed_outcome(result, canceled, cx.output_limit))
}

pub(super) async fn recovery_outcome(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
    canceled: bool,
    output_limit: usize,
) -> Result<RecoveryOutcome> {
    if let Some(result) = session.fixed_prompt_completion(ticket).await? {
        return Ok(completed_outcome(result, canceled, output_limit));
    }
    let cx = RecoveryContext {
        session,
        ticket,
        output_limit,
    };
    match session.close_fixed_admission(ticket).await? {
        FixedAdmissionOutcome::Admitted { prompt_sequence } => {
            handle_admitted_closure(&cx, prompt_sequence, canceled).await
        }
        FixedAdmissionOutcome::Closed { .. } | FixedAdmissionOutcome::Fenced { .. } => {
            Ok(interrupted_outcome(canceled, AdmissionPhase::Closed, None))
        }
        FixedAdmissionOutcome::Pending => anyhow::bail!("admission closure unconfirmed"),
    }
}
