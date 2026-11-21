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

// qual:allow(iosp) reason: "The sweep gate must check preconditions, cancel intent and the steered drain in one ordered pass; reordering it lets a drain hold preempt a cancel."
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
    if let Some(active) = authority.context.document.state.active.as_ref() {
        let task_id = active.snapshot.task.id.clone();
        // A user cancel intent must not wait out the drain: fall through to
        // the interrupt+settle below, which resolves it.
        if active.cancel.is_none()
            && completion_pending_steered_drain(session, active.snapshot.user_msg_seq).await?
        {
            // Hold for a live worker to answer the steered rows, bounded by
            // HEAD_DRAIN_TIMEOUT; after that the sweep falls through to the
            // ordinary owner-loss interrupt+settle instead of wedging the
            // task, its CancelTask confirmations and the context.
            if runner.hold_steered_drain(&task_id) {
                return Ok(());
            }
        } else {
            runner.clear_steered_drain_hold(&task_id);
        }
    }
    let outcome = resolve_recovery_output(runner, session, authority).await?;
    apply_terminal_settlement(runner, session, authority, outcome).await
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

async fn verify_hitl_continuation_progress(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
    active: &ActiveTask,
) -> Result<()> {
    if active.cancel.is_none() && active.hitl_checkpoint.is_some() {
        anyhow::ensure!(
            hitl_continuation_has_progress(session, ticket, active).await?,
            "HITL continuation requires progress recovery"
        );
    }
    Ok(())
}

/// An outstanding steered drain legitimately defers completion (the
/// invocation is not durably complete until the drain settles), and that
/// state must reach the bounded drain hold in the sweep — and, after it
/// expires, the ordinary owner-loss interrupt+settle — instead of failing
/// the progress gate forever. Only a missing admission or terminal proof
/// is a recovery failure.
// qual:allow(iosp) reason: "The two progress probes must stay in one ordered pass: the durable completion proof first, then the live steered drain. Splitting them lets a completion check skip the drain hold that recovery depends on."
async fn hitl_continuation_has_progress(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
    active: &ActiveTask,
) -> Result<bool> {
    let completion = session.fixed_prompt_completion(ticket).await?;
    if completion.is_some() {
        return Ok(true);
    }
    completion_pending_steered_drain(session, active.snapshot.user_msg_seq).await
}

/// The task's own prompt already has its terminal proof, but steered rows
/// appended into the same invocation (issue #2774) are still owed
/// continuation-round answers. A recovering replica must not settle the
/// task on the root answer in that window; the next reconciliation observes
/// the drained head and settles with the steered response. The check is
/// log-only, so broker faults cannot wedge or bypass recovery.
async fn completion_pending_steered_drain(
    session: &NatsSession,
    prompt_sequence: u64,
) -> Result<bool> {
    // Raw log entries: terminators (TurnEnd/Cancel/Error) only exist there.
    // The mutation-applied view drops TurnEnd rows, which would hide the
    // covering terminator and make an outstanding drain look settled.
    let metadata = session.metadata_store();
    let entries = harnx_runtime::nats_session_log::NatsSessionLog::new_with_replicas(
        metadata.jetstream().clone(),
        session.storage_key(),
        metadata.replicas(),
    )
    .load_events_latest_required_async()
    .await?;
    harnx_runtime::nats_session::steered_rows_outstanding(&entries, prompt_sequence)
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

// qual:allow(iosp) reason: "Reads the completion proof, HITL checkpoint and output limit, then delegates; each read names a different settlement input."
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
    if let Some(id) = authority
        .context
        .document
        .state
        .active
        .as_ref()
        .map(|active| active.snapshot.task.id.clone())
    {
        // A settled task owes no steered drain anymore; drop any hold so the
        // map does not pin the entry until its expiry purge.
        runner.clear_steered_drain_hold(&id);
    }
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
