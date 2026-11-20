use super::fixture::*;
use super::helpers::*;
use super::stub::*;

use anyhow::{Context, Result};
use harnx_core::session::SessionLogEntry;

use harnx_runtime::surfaces::SurfaceActionOutcome;

use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_surface_is_durable_and_absent_from_model_input() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;

    let entries = fixture.entries().await?;
    let (publish_seq, surface_id) =
        first_published_surface(&entries).context("surface published durably")?;
    assert_eq!(surface_id, "deploy-1");

    // The round completed and the model kept going past the publication: the
    // turn ended and produced its final assistant text.
    let assistant_done = entries.iter().any(|(_, entry)| {
        matches!(entry, SessionLogEntry::Message { content, .. }
            if content.to_text() == "all done")
    });
    assert!(assistant_done, "turn completed while surface outstanding");

    // Every model-visible round is free of the surface envelope.
    for (round, texts) in model_round_texts().iter().enumerate() {
        for text in texts {
            assert!(
                !text.contains("application/a2ui+json") && !text.contains("deploy-1"),
                "round {round} leaked the surface to the model: {text}"
            );
        }
    }
    // The model-visible tool result still carries the plain text block.
    assert!(
        model_round_texts()[1]
            .iter()
            .any(|text| text.contains("prepared")),
        "model saw the tool's visible output"
    );

    // An unknown surface generation is rejected without side effects.
    let entries_before = fixture.entries().await?.len();
    let forged = action_submission("not-a-surface", publish_seq, "note_act", None);
    assert_eq!(
        fixture.session.submit_surface_action(&forged).await?,
        SurfaceActionOutcome::UnknownSurface
    );
    let stale = action_submission(&surface_id, publish_seq + 999, "note_act", None);
    assert_eq!(
        fixture.session.submit_surface_action(&stale).await?,
        SurfaceActionOutcome::UnknownSurface
    );
    assert_eq!(fixture.entries().await?.len(), entries_before);

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_invisible_action_leaves_durable_record_only() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;

    let submission = action_submission(&surface_id, publish_seq, "invisible_act", None);
    assert_eq!(
        fixture.session.submit_surface_action(&submission).await?,
        SurfaceActionOutcome::Applied
    );
    // Identical retry is acknowledged without a second record.
    assert_eq!(
        fixture.session.submit_surface_action(&submission).await?,
        SurfaceActionOutcome::AlreadyApplied
    );

    let entries = fixture.entries().await?;
    let records = surface_action_entries(&entries);
    assert_eq!(records.len(), 1, "first-wins: one durable record");
    let SessionLogEntry::SurfaceAction {
        visibility, action, ..
    } = records[0]
    else {
        panic!("surface action record");
    };
    assert_eq!(visibility, "invisible");
    assert_eq!(action, "invisible_act");
    // Invisible actions have no model effect: no note text recorded.
    let SessionLogEntry::SurfaceAction { note_text, .. } = records[0] else {
        unreachable!()
    };
    assert!(note_text.is_none());

    // The next model call never sees it: run a fresh turn and scan requests.
    reset_model_state();
    run_single_turn(&fixture, "continue").await?;
    for texts in model_round_texts() {
        for text in &texts {
            assert!(!text.contains("invisible_act"), "leaked: {text}");
        }
    }

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_note_action_reaches_next_model_call_without_new_turn() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("gated").await?;
    let entries = fixture.entries().await?;
    let before = entries.last().map_or(0, |(seq, _)| *seq);
    fixture
        .session
        .clone()
        .with_external_admission()
        .enqueue_text("prepare the deployment")
        .await?
        .into_activation_result()?;

    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;
    // The turn is still running here: the gated stub keeps calling the
    // counter tool until we submit.
    let submission = action_submission(
        &surface_id,
        publish_seq,
        "note_act",
        Some("deployment reviewed by the operator"),
    );
    assert_eq!(
        fixture.session.submit_surface_action(&submission).await?,
        SurfaceActionOutcome::Applied
    );
    GATED_SUBMITTED.store(true, Ordering::SeqCst);

    wait_for_turn_end_after(&fixture, before).await?;
    let entries = fixture.entries().await?;
    let records = surface_action_entries(&entries);
    assert_eq!(records.len(), 1);
    let SessionLogEntry::SurfaceAction {
        visibility,
        note_text,
        ..
    } = records[0]
    else {
        panic!("surface action record");
    };
    assert_eq!(visibility, "note");
    assert_eq!(
        note_text.as_deref(),
        Some("deployment reviewed by the operator")
    );

    // The note reached the model inside the SAME turn: no extra user message
    // was appended, and a later round's request contains the note text.
    let user_rows = user_message_texts(&entries);
    assert_eq!(
        user_rows.len(),
        1,
        "note did not start a new turn: user rows {user_rows:?}"
    );
    let rounds = model_round_texts();
    let last_round = rounds.last().context("final round")?;
    assert!(
        last_round
            .iter()
            .any(|text| text.contains("deployment reviewed by the operator")),
        "note missing from the model's next call: {last_round:?}"
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_note_action_idle_session_delivered_on_next_turn() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;

    // The turn is over and the worker released the session: the client falls
    // back to recording the action itself, and the next turn's fold delivers
    // the note.
    let submission = action_submission(
        &surface_id,
        publish_seq,
        "note_act",
        Some("idle note from the operator"),
    );
    assert_eq!(
        fixture.session.submit_surface_action(&submission).await?,
        SurfaceActionOutcome::Applied
    );

    reset_model_state();
    run_single_turn(&fixture, "continue").await?;
    let rounds = model_round_texts();
    assert!(
        rounds[0]
            .iter()
            .any(|text| text.contains("idle note from the operator")),
        "idle note not delivered at the next turn's first model call: {:?}",
        rounds[0]
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_turn_action_starts_a_turn_with_the_user_message() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;

    let entries = fixture.entries().await?;
    let before = entries.last().map_or(0, |(seq, _)| *seq);
    let submission = action_submission(
        &surface_id,
        publish_seq,
        "turn_act",
        Some("what should we watch during rollout?"),
    );
    assert_eq!(
        fixture.session.submit_surface_action(&submission).await?,
        SurfaceActionOutcome::Applied
    );

    // The action's user message starts a new turn on the idle session.
    wait_for_turn_end_after(&fixture, before).await?;
    let entries = fixture.entries().await?;
    assert!(
        user_message_texts(&entries)
            .iter()
            .any(|text| text == "what should we watch during rollout?"),
        "turn action enqueued its user message"
    );
    assert!(
        entries.iter().any(|(_, entry)| {
            matches!(entry, SessionLogEntry::Message { content, .. }
                if content.to_text() == "all done")
        }),
        "the started turn completed"
    );

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}
