//! Client-side acknowledgement discrimination for surface actions (F6):
//! `lease_lost` before the acknowledgement is a mixed-version tolerance
//! case the client resends past (F1 removed it from current workers), while
//! the same reply after the acknowledgement is terminal — the owning
//! worker lost the attempt with nothing recorded, and a resend could re-run
//! a non-idempotent tool under the new holder.

use super::fixture::*;
use super::helpers::*;
use super::stub::*;

use crate::worker::EnvGuard;
use anyhow::Result;
use futures_util::StreamExt;
use harnx_runtime::surfaces::{SurfaceActionOutcome, SurfaceActionReply};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A scripted stand-in for the session's control listener: every received
/// `SurfaceAction` command answers with the next batch of replies, in
/// order. Counting the commands makes the client's resend behavior an
/// assertable fact instead of a timing guess.
async fn scripted_control_listener(
    fixture: &SurfaceFixture,
    script: Vec<Vec<SurfaceActionReply>>,
) -> Result<Arc<AtomicUsize>> {
    let subject = harnx_runtime::nats_worker::control_subject(fixture.session.storage_key());
    let mut commands = fixture.client.subscribe(subject).await?;
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    let publisher = fixture.client.clone();
    tokio::spawn(async move {
        while let Some(message) = commands.next().await {
            let Ok(harnx_runtime::nats_worker::ControlCommand::SurfaceAction { .. }) =
                harnx_runtime::nats_worker::ControlCommand::from_bytes(&message.payload)
            else {
                continue;
            };
            let seen = counter.fetch_add(1, Ordering::SeqCst);
            let Some(reply_to) = message.reply else {
                continue;
            };
            let Some(replies) = script.get(seen) else {
                continue;
            };
            for reply in replies {
                let payload = serde_json::to_vec(reply).expect("serialize scripted reply");
                if publisher
                    .publish(reply_to.clone(), payload.into())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = publisher.flush().await;
        }
    });
    // The subscription must be live before the client submits, or the
    // first command (and its scripted answer) could be missed.
    fixture.client.flush().await?;
    Ok(count)
}

/// Submit `note_act` against the fixture's idle session with only the
/// scripted listener answering, and return the client's outcome. Also
/// asserts the invariant both tests share: while a worker owns the
/// attempt, the client must never fall back to a local idle append.
async fn submit_against_scripted_worker(
    fixture: &SurfaceFixture,
    surface_id: &str,
    publish_seq: u64,
) -> Result<SurfaceActionOutcome> {
    let entries_before = fixture.entries().await?.len();
    let submission = action_submission(surface_id, publish_seq, "note_act", None);
    let outcome = fixture.session.submit_surface_action(&submission).await?;
    assert_eq!(
        fixture.entries().await?.len(),
        entries_before,
        "the attempt must leave no local idle append behind"
    );
    Ok(outcome)
}

/// F6 + F1 regression: a pre-acknowledgement `lease_lost` — the wire
/// refusal a stale mixed-version worker still sends, and the one current
/// workers no longer send — is not an outcome. The client resends past it,
/// acknowledges the holder's `Pending`, and resolves on the final reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_pre_ack_lease_lost_is_resend_hint_until_holder_answers() -> Result<()> {
    reset_model_state();
    let (fixture, surface_id, publish_seq) = single_surface().await?;
    // Headroom for the one resend the ignored refusal costs: the fixture's
    // short budget exists for the idle-fallback tests, not for this one.
    let _budget = EnvGuard::set("HARNX_SURFACE_ACTION_RECOVERY_TIMEOUT_MS", "4000");

    let commands = scripted_control_listener(
        &fixture,
        vec![
            vec![SurfaceActionReply::rejected(
                SurfaceActionOutcome::LeaseLost,
            )],
            vec![
                SurfaceActionReply::rejected(SurfaceActionOutcome::Pending),
                SurfaceActionReply::rejected(SurfaceActionOutcome::Applied),
            ],
        ],
    )
    .await?;

    let outcome = submit_against_scripted_worker(&fixture, &surface_id, publish_seq).await?;

    assert_eq!(outcome, SurfaceActionOutcome::Applied);
    assert_eq!(
        commands.load(Ordering::SeqCst),
        2,
        "the ignored pre-ack refusal must cost exactly one resend"
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// F6: `lease_lost` after the acknowledgement is terminal. The owning
/// worker proved its lease when it acknowledged, so this attempt died with
/// nothing recorded; the client must return it instead of resending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_acked_lease_lost_is_terminal_without_resend() -> Result<()> {
    reset_model_state();
    let (fixture, surface_id, publish_seq) = single_surface().await?;

    let commands = scripted_control_listener(
        &fixture,
        vec![vec![
            SurfaceActionReply::rejected(SurfaceActionOutcome::Pending),
            SurfaceActionReply::rejected(SurfaceActionOutcome::LeaseLost),
        ]],
    )
    .await?;

    let outcome = submit_against_scripted_worker(&fixture, &surface_id, publish_seq).await?;

    assert_eq!(outcome, SurfaceActionOutcome::LeaseLost);
    assert_eq!(
        commands.load(Ordering::SeqCst),
        1,
        "a terminal post-ack lease_lost must not be resent"
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}
