use crate::store::context::ActiveTask;
use a2a_lf::TaskState;
use anyhow::{Context, Result};
use harnx_runtime::{nats_session::fixed_admission::FixedAdmissionTicket, NatsSession};

use super::outcome::{recovery_outcome, recovery_status};
use crate::{
    runner::{
        authority::{stage_terminal, OwnedTask},
        event_map::artifact,
        Runner,
    },
    store::context::AdmissionPhase,
};

pub(super) async fn recover_admission(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
) -> Result<()> {
    if validate_admission_preconditions(runner, session, authority)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let outcome = resolve_recovery_output(runner, session, authority).await?;
    apply_terminal_settlement(runner, session, authority, outcome).await
}

async fn verify_hitl_continuation_progress(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
    active: &ActiveTask,
) -> Result<()> {
    if active.cancel.is_none() && active.hitl_checkpoint.is_some() {
        anyhow::ensure!(
            session.fixed_prompt_completion(ticket).await?.is_some(),
            "HITL continuation requires progress recovery"
        );
    }
    Ok(())
}

async fn validate_admission_preconditions(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
) -> Result<Option<()>> {
    authority
        .flush_pending(&runner.store, session.storage_key())
        .await?;
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("missing recovery task")?;
    if active.snapshot.task.status.state.is_terminal() {
        authority.project_terminal(&runner.store, session).await?;
        return Ok(Some(()));
    }
    let ticket = authority.ticket(session.storage_key())?;
    verify_hitl_continuation_progress(session, &ticket, active).await?;
    runner.repair_hitl_mapping(session, authority).await?;
    Ok(None)
}

struct SettlementOutcome {
    state: TaskState,
    text: Option<String>,
    phase: AdmissionPhase,
    prompt: Option<u64>,
}

async fn recover_completed_hitl_output(
    runner: &Runner,
    storage_key: &str,
    prompt: Option<u64>,
    output_limit: usize,
) -> Result<Option<String>> {
    let recovered = crate::runner::durable_output::load_invocation_text(
        &runner.store,
        storage_key,
        prompt.context("completed HITL prompt missing")?,
        None,
    )
    .await?;
    let (_, text) = crate::runner::limits::bounded_outcome(
        (TaskState::Completed, Some(recovered)),
        output_limit,
    );
    Ok(text)
}

async fn resolve_recovery_output(
    runner: &Runner,
    session: &NatsSession,
    authority: &OwnedTask,
) -> Result<SettlementOutcome> {
    let active = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .context("missing recovery task")?;
    let ticket = authority.ticket(session.storage_key())?;
    let has_hitl = active.hitl_checkpoint.is_some();
    let output_limit =
        crate::runner::limits::task_output_limit(&runner.store, &authority.context).await?;
    let (state, mut text, phase, prompt) =
        recovery_outcome(session, &ticket, active.cancel.is_some(), output_limit).await?;
    if has_hitl && state == TaskState::Completed {
        text = recover_completed_hitl_output(runner, session.storage_key(), prompt, output_limit)
            .await?;
    }
    Ok(SettlementOutcome {
        state,
        text,
        phase,
        prompt,
    })
}

async fn apply_terminal_settlement(
    runner: &Runner,
    session: &NatsSession,
    authority: &mut OwnedTask,
    outcome: SettlementOutcome,
) -> Result<()> {
    if let Some(text) = outcome
        .text
        .as_deref()
        .filter(|_| outcome.state == TaskState::Completed)
    {
        super::artifact::recover_artifact(authority, &runner.store, session, text).await?;
    }
    authority
        .update(&runner.store, session.storage_key(), |active| {
            active.snapshot.task.status = recovery_status(
                outcome.state.clone(),
                outcome.text.as_deref(),
                active.cancel.is_some(),
            );
            if outcome.state == TaskState::Completed {
                active.snapshot.task.artifacts =
                    outcome.text.clone().map(|text| vec![artifact(text)]);
                if let Some(message) = active.snapshot.task.status.message.clone() {
                    active
                        .snapshot
                        .task
                        .history
                        .get_or_insert_with(Vec::new)
                        .push(message);
                }
            }
            active.admission.phase = outcome.phase.clone();
            active.admission.prompt_sequence = outcome.prompt;
            if let Some(prompt) = outcome.prompt {
                active.snapshot.user_msg_seq = prompt;
            }
            active.stop_confirmed = true;
            stage_terminal(active);
        })
        .await?;
    authority.project_terminal(&runner.store, session).await
}
