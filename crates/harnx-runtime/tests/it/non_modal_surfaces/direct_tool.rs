use super::fixture::*;
use super::helpers::*;
use super::stub::*;

use anyhow::Result;
use harnx_core::session::SessionLogEntry;

use harnx_runtime::surfaces::SurfaceActionOutcome;

use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_direct_tool_action_runs_once_and_notes_the_model() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;

    let submission = action_submission(&surface_id, publish_seq, "tool_act", None);
    assert_eq!(
        fixture.session.submit_surface_action(&submission).await?,
        SurfaceActionOutcome::Applied
    );
    // Default visibility for a direct tool call is `note`; the side-effecting
    // tool ran exactly once.
    assert_eq!(fixture.counter.load(Ordering::SeqCst), 1);

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
    assert!(
        note_text
            .as_deref()
            .is_some_and(|text| text.contains("count")),
        "generated note quotes the tool result: {note_text:?}"
    );

    // The note (with the tool result) reaches the next model call.
    reset_model_state();
    run_single_turn(&fixture, "continue").await?;
    assert!(
        model_round_texts()[0]
            .iter()
            .any(|text| text.contains("count")),
        "direct tool note missing from next model call"
    );

    // Identical retry: already applied, the tool never re-runs.
    assert_eq!(
        fixture.session.submit_surface_action(&submission).await?,
        SurfaceActionOutcome::AlreadyApplied
    );
    assert_eq!(fixture.counter.load(Ordering::SeqCst), 1);
    // A different action on the same generation conflicts.
    let conflicting = action_submission(&surface_id, publish_seq, "invisible_act", None);
    assert_eq!(
        fixture.session.submit_surface_action(&conflicting).await?,
        SurfaceActionOutcome::Conflicting
    );
    assert_eq!(fixture.counter.load(Ordering::SeqCst), 1);

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_invalid_actions_rejected_without_side_effects() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;
    let entries_before = fixture.entries().await?.len();

    // Undeclared action (deny included): nothing on this surface names it.
    assert_eq!(
        fixture
            .session
            .submit_surface_action(&action_submission(&surface_id, publish_seq, "deny", None))
            .await?,
        SurfaceActionOutcome::UndeclaredAction
    );
    assert_eq!(
        fixture
            .session
            .submit_surface_action(&action_submission(
                &surface_id,
                publish_seq,
                "made_up",
                None
            ))
            .await?,
        SurfaceActionOutcome::UndeclaredAction
    );
    // Invalid visibility in the declaration.
    assert_eq!(
        fixture
            .session
            .submit_surface_action(&action_submission(
                &surface_id,
                publish_seq,
                "loud_act",
                None
            ))
            .await?,
        SurfaceActionOutcome::InvalidVisibility
    );
    // Unauthorized target tool: never declared on the session's toolset.
    assert_eq!(
        fixture
            .session
            .submit_surface_action(&action_submission(
                &surface_id,
                publish_seq,
                "unauth_act",
                None
            ))
            .await?,
        SurfaceActionOutcome::UnauthorizedTool
    );
    // Unknown surface generation.
    assert_eq!(
        fixture
            .session
            .submit_surface_action(&action_submission("forged", publish_seq, "note_act", None))
            .await?,
        SurfaceActionOutcome::UnknownSurface
    );

    // Nothing was appended, dispatched or enqueued for any rejection.
    assert_eq!(fixture.entries().await?.len(), entries_before);
    assert_eq!(fixture.counter.load(Ordering::SeqCst), 0);
    let entries = fixture.entries().await?;
    assert!(surface_action_entries(&entries).is_empty());
    // The model never saw anything from the rejected submissions.
    assert!(user_message_texts(&entries).len() == 1);

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}

// ---------------------------------------------------------------------------
// T5.1 review follow-up: at-most-once execution (B1), fail-closed lease state
// (C8/P3), turn recovery after a lost reply (C7), invisible isolation (B2),
// user-only scrubbing of direct-tool summaries (P2).
// ---------------------------------------------------------------------------

/// P2: the direct-tool summary quoted into model-visible note text never
/// contains user-audience surfaces from the target's output.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_modal_direct_tool_summary_scrubs_user_only_surfaces() -> Result<()> {
    reset_model_state();
    let fixture = SurfaceFixture::start("single").await?;
    run_single_turn(&fixture, "prepare the deployment").await?;
    let (surface_id, publish_seq) = fixture.wait_surface_published().await?;

    let submission = action_submission(&surface_id, publish_seq, "useronly_act", None);
    assert_eq!(
        fixture.session.submit_surface_action(&submission).await?,
        SurfaceActionOutcome::Applied
    );
    let entries = fixture.entries().await?;
    let actions = recorded_actions(&entries);
    assert_eq!(actions.len(), 1);
    let (_, note_text) = &actions[0];
    let note_text = note_text.as_deref().expect("note text recorded");
    assert!(
        note_text.contains("useronly-ok"),
        "visible part kept: {note_text}"
    );
    assert!(
        !note_text.contains("useronly-secret-card") && !note_text.contains("application/a2ui+json"),
        "user-only surface must be scrubbed from model text: {note_text}"
    );

    // Nothing reaches the provider on the following turn either.
    reset_model_state();
    run_single_turn(&fixture, "continue").await?;
    for (round, texts) in model_round_texts().iter().enumerate() {
        for text in texts {
            assert!(
                !text.contains("useronly-secret-card"),
                "round {round} leaked the user-only surface: {text}"
            );
        }
    }

    fixture.daemon.abort();
    let _ = fixture.daemon.await;
    Ok(())
}
