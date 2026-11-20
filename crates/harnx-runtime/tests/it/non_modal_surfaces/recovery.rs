use super::fixture::*;
use super::helpers::*;
use super::stub::*;
use crate::worker::CI_SAFE_TIMEOUT;
use anyhow::Result;
use harnx_core::session::SessionLogEntry;

use harnx_runtime::surfaces::{SurfaceActionOutcome, SurfaceActionSubmission};

use std::sync::atomic::Ordering;
use std::time::Duration;

/// C7: the first attempt's reply is lost — a raw control command whose reply
/// inbox nobody reads — and a retried submission sees only the durable
/// record. The `turn` effect is recovered from the record and delivered
/// exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_turn_action_delivered_after_lost_reply() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("steered").await?;
    let before = start_running_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;

    let submission = action_submission(
        &surface_id,
        publish_seq,
        "turn_tool_act",
        Some(STEER_TURN_MARKER),
    );

    // The abandoned first attempt: the worker runs the parked tool, appends
    // the record and replies to an inbox nobody reads.
    let command = harnx_runtime::nats_worker::ControlCommand::SurfaceAction {
        submission: submission.clone(),
    };
    let subject = harnx_runtime::nats_worker::control_subject(fixture.session.storage_key());
    let dead_inbox = fixture.client.new_inbox();
    fixture
        .client
        .publish_with_reply(subject, dead_inbox, command.to_bytes()?.into())
        .await?;
    wait_slow_started(&fixture).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    fixture.slow_release.store(true, Ordering::SeqCst);
    // The record is durable but its reply is gone.
    let deadline = std::time::Instant::now() + CI_SAFE_TIMEOUT;
    while surface_action_entries(&fixture.entries().await?).is_empty() {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the abandoned attempt never recorded the action"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fixture.slow_counter.load(Ordering::SeqCst), 1);

    // The retry — a fresh submission that can only see the record — must
    // classify as already applied and still deliver the turn text.
    let outcome = fixture.session.submit_surface_action(&submission).await?;
    assert_eq!(
        outcome,
        SurfaceActionOutcome::AlreadyApplied,
        "the retry classifies from the record, not the lost reply"
    );

    // The recovered turn text steers the running turn, exactly once.
    wait_for_turn_end_after(&fixture, before).await?;
    let entries = fixture.entries().await?;
    let delivered = user_message_texts(&entries)
        .into_iter()
        .filter(|text| text == STEER_TURN_MARKER)
        .count();
    assert_eq!(
        delivered, 1,
        "turn text must be delivered exactly once after a lost reply"
    );
    assert_eq!(
        recorded_actions(&entries),
        vec![(
            "turn_tool_act/turn".to_string(),
            Some(STEER_TURN_MARKER.into())
        )]
    );
    assert!(entries.iter().any(|(_, entry)| {
        matches!(entry, SessionLogEntry::Message { content, .. }
            if content.to_text() == "all done")
    }));

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

/// B2: an `invisible` action mid-turn never reaches the model — no note
/// queue row, no generated action text — while a `note` action on another
/// generation still arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_invisible_action_mid_turn_never_reaches_model() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("gated").await?;
    let before = start_running_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;
    let (note_id, note_seq) = wait_surface_generation(&fixture, "note-2").await?;

    let invisible = SurfaceActionSubmission {
        surface_id,
        publish_seq,
        response: harnx_core::interaction::InteractionResponse {
            action: "invisible_act".into(),
            data: serde_json::Map::new(),
            note: None,
        },
        // The submitted marker rides the record only: an invisible action
        // has no model effect, so nothing may ever quote it back.
        user_message: Some(INVISIBLE_MARKER.into()),
    };
    assert_eq!(
        fixture.session.submit_surface_action(&invisible).await?,
        SurfaceActionOutcome::Applied
    );

    // End the turn through a note on the other published generation.
    let note = action_submission(&note_id, note_seq, "note_act", Some(GATED_NOTE_MARKER));
    assert_eq!(
        fixture.session.submit_surface_action(&note).await?,
        SurfaceActionOutcome::Applied
    );
    GATED_SUBMITTED.store(true, Ordering::SeqCst);
    wait_for_turn_end_after(&fixture, before).await?;

    // No provider request ever carried the invisible submission's data, and
    // no generated invisible-action text leaked either.
    for (round, texts) in model_round_texts().iter().enumerate() {
        for text in texts {
            assert!(
                !text.contains(INVISIBLE_MARKER),
                "round {round} leaked invisible data: {text}"
            );
            assert!(
                !text.contains("[surface action 'invisible_act'"),
                "round {round} leaked invisible action text: {text}"
            );
        }
    }
    // The note itself arrived mid-turn.
    let entries = fixture.entries().await?;
    let actions = recorded_actions(&entries);
    assert_eq!(actions.len(), 2, "invisible + note records: {actions:?}");
    assert!(
        actions
            .iter()
            .any(|(action, note_text)| action == "invisible_act/invisible" && note_text.is_none()),
        "invisible records stay text-free: {actions:?}"
    );
    assert!(
        actions
            .iter()
            .any(|(action, note_text)| action == "note_act/note"
                && note_text.as_deref() == Some(GATED_NOTE_MARKER)),
        "note still delivered: {actions:?}"
    );
    // The note is the only non-prompt user row: the invisible action did
    // not start or join a turn.
    assert_eq!(
        user_message_texts(&entries).len(),
        1,
        "no extra user rows from invisible or note actions"
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}
