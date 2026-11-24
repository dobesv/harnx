use super::helpers::*;
use crate::worker::{EnvGuard, CI_SAFE_TIMEOUT};
use anyhow::{Context, Result};
use futures_util::StreamExt;

use harnx_runtime::surfaces::SurfaceActionOutcome;

use std::sync::atomic::Ordering;
use std::time::Duration;

/// B1: a slow direct-tool action parks far past the old ack timeout, then
/// duplicate commands from old-style resending clients all coalesce against
/// the durable record — one dispatch, one record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_resends_of_slow_direct_tool_dispatch_once() -> Result<()> {
    let (fixture, surface_id, publish_seq) = gated_surface().await?;

    // The single execution parks; a new-style client is acknowledged up
    // front and never resends, never mind falls back to its own execution
    // — hold the window past the client's whole recovery budget so a
    // buggy fallback would have had its chance to re-run the target.
    let submitted_at = std::time::Instant::now();
    let submit = start_parked_slow_action(&fixture, &surface_id, publish_seq).await?;
    hold_parked_past_recovery_budget(&fixture, submitted_at).await?;
    fixture.slow_release.store(true, Ordering::SeqCst);

    let outcome = submit.await??;
    assert!(
        outcome.is_applied(),
        "the accepted attempt must resolve, got {outcome:?}"
    );
    assert_eq!(
        fixture.slow_counter.load(Ordering::SeqCst),
        1,
        "the parked window must not have re-run the target tool"
    );
    let entries = fixture.entries().await?;
    assert_eq!(surface_action_entries(&entries).len(), 1, "one record");

    // Old-style clients resend duplicate commands while no reply is
    // outstanding-readable: every duplicate classifies from the durable
    // record, and none re-executes.
    let command = harnx_runtime::nats_worker::ControlCommand::SurfaceAction {
        submission: action_submission(&surface_id, publish_seq, "slow_act", None),
    };
    let subject = harnx_runtime::nats_worker::control_subject(fixture.session.storage_key());
    let inbox = fixture.client.new_inbox();
    let mut replies = fixture.client.subscribe(inbox.clone()).await?;
    for _ in 0..3 {
        fixture
            .client
            .publish_with_reply(subject.clone(), inbox.clone(), command.to_bytes()?.into())
            .await?;
    }
    fixture.client.flush().await?;
    let mut outcomes = Vec::new();
    while outcomes.len() < 3 {
        let message = replies
            .next()
            .await
            .context("duplicate command reply missing")?;
        let answer: harnx_runtime::surfaces::SurfaceActionReply =
            serde_json::from_slice(&message.payload)?;
        if answer.outcome != SurfaceActionOutcome::Pending {
            outcomes.push(answer.outcome);
        }
    }
    assert!(
        outcomes
            .iter()
            .all(|outcome| *outcome == SurfaceActionOutcome::AlreadyApplied),
        "duplicates must classify from the record: {outcomes:?}"
    );
    assert_eq!(fixture.slow_counter.load(Ordering::SeqCst), 1);
    let entries = fixture.entries().await?;
    assert_eq!(surface_action_entries(&entries).len(), 1);

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// B1: two concurrent identical submissions dispatch the target once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_concurrent_identical_submits_dispatch_once() -> Result<()> {
    let (fixture, surface_id, publish_seq) = gated_surface().await?;

    // Barrier instead of a fixed sleep: wait until both submissions'
    // commands are on the control subject, so the race is provably live
    // (one execution parked, the other queued at or racing for the
    // generation gate) before the parked window ends.
    let commands = subscribe_control_commands(&fixture).await?;
    let submission = action_submission(&surface_id, publish_seq, "slow_act", None);
    let first = spawn_submission(fixture.session.clone(), submission.clone());
    let second = spawn_submission(fixture.session.clone(), submission.clone());

    wait_slow_started(&fixture).await?;
    wait_control_commands(&commands, 2).await?;
    fixture.slow_release.store(true, Ordering::SeqCst);

    let (first, second) = (first.await??, second.await??);
    assert!(
        first.is_applied() && second.is_applied(),
        "both submissions resolve against one acceptance: {first:?} / {second:?}"
    );
    assert_eq!(fixture.slow_counter.load(Ordering::SeqCst), 1);
    let entries = fixture.entries().await?;
    assert_eq!(surface_action_entries(&entries).len(), 1);

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// B1: first-wins against a concurrent conflicting submit of the same
/// generation — one record, the other side classifies `conflicting`, and
/// the losing tool action never runs its target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_concurrent_conflicting_submit_conflicts() -> Result<()> {
    let (fixture, surface_id, publish_seq) = gated_surface().await?;

    let slow = action_submission(&surface_id, publish_seq, "slow_act", None);
    let other = action_submission(&surface_id, publish_seq, "invisible_act", None);
    let slow_task = spawn_submission(fixture.session.clone(), slow.clone());
    let other_task = spawn_submission(fixture.session.clone(), other);

    // Whichever action wins the generation gate first applies; the other
    // loses. Wait until the race resolves, then release the parked tool.
    let deadline = std::time::Instant::now() + CI_SAFE_TIMEOUT;
    loop {
        let decided = fixture.slow_counter.load(Ordering::SeqCst) > 0
            || !surface_action_entries(&fixture.entries().await?).is_empty();
        if decided || std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fixture.slow_release.store(true, Ordering::SeqCst);

    let (slow_outcome, other_outcome) = (slow_task.await??, other_task.await??);
    let mut outcomes = vec![slow_outcome, other_outcome];
    outcomes.sort_by_key(|outcome| format!("{outcome:?}"));
    assert_eq!(
        outcomes,
        vec![
            SurfaceActionOutcome::Applied,
            SurfaceActionOutcome::Conflicting
        ],
        "exactly one submit wins the generation"
    );
    assert!(fixture.slow_counter.load(Ordering::SeqCst) <= 1);
    let entries = fixture.entries().await?;
    assert_eq!(surface_action_entries(&entries).len(), 1);

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// B1: an unrelated append during execution costs one CAS conflict, one
/// reload — and still exactly one dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_unrelated_append_during_execution_dispatches_once() -> Result<()> {
    let (fixture, surface_id, publish_seq) = gated_surface().await?;

    let submit = start_parked_slow_action(&fixture, &surface_id, publish_seq).await?;
    append_unrelated_row(&fixture, "unrelated concurrent log write").await?;
    fixture.slow_release.store(true, Ordering::SeqCst);

    let outcome = submit.await??;
    assert!(
        outcome.is_applied(),
        "conflict must be retried against the fresh tail, got {outcome:?}"
    );
    assert_eq!(fixture.slow_counter.load(Ordering::SeqCst), 1);
    let entries = fixture.entries().await?;
    assert_eq!(surface_action_entries(&entries).len(), 1);

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// B1: CAS exhaustion answers with an explicit `append_failed` outcome —
/// never silence, never a re-run of the tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_cas_exhaustion_replies_append_failed() -> Result<()> {
    let _rounds = EnvGuard::set("HARNX_SURFACE_ACTION_CAS_ROUNDS", "1");
    let (fixture, surface_id, publish_seq) = gated_surface().await?;

    let submit = start_parked_slow_action(&fixture, &surface_id, publish_seq).await?;
    append_unrelated_row(&fixture, "unrelated concurrent log write").await?;
    fixture.slow_release.store(true, Ordering::SeqCst);

    let outcome = submit.await??;
    assert_eq!(outcome, SurfaceActionOutcome::AppendFailed);
    assert_eq!(fixture.slow_counter.load(Ordering::SeqCst), 1);
    let entries = fixture.entries().await?;
    assert!(
        surface_action_entries(&entries).is_empty(),
        "nothing durable after CAS exhaustion"
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// B1, idle path: the client-side fallback also runs the tool once, outside
/// its CAS loop, and reports exhaustion as an explicit error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_idle_cas_exhaustion_errors_without_record() -> Result<()> {
    let _rounds = EnvGuard::set("HARNX_SURFACE_ACTION_CAS_ROUNDS", "1");
    let (fixture, surface_id, publish_seq) = single_surface().await?;

    let submit = start_parked_slow_action(&fixture, &surface_id, publish_seq).await?;
    append_unrelated_row(&fixture, "unrelated concurrent log write").await?;
    fixture.slow_release.store(true, Ordering::SeqCst);

    let result = submit.await?;
    let error = result.expect_err("exhaustion must be an explicit error");
    assert!(
        format!("{error:#}").contains("CAS race"),
        "unexpected error: {error:#}"
    );
    assert_eq!(fixture.slow_counter.load(Ordering::SeqCst), 1);
    let entries = fixture.entries().await?;
    assert!(surface_action_entries(&entries).is_empty());

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}
