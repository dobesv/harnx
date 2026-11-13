//! CodeRabbit round on PR #2694: O1 (steering turn-end race heals within the
//! first call) and O2 (existing-row replay keeps its admitted invocation
//! identity), reproduced deterministically at the reviewed seams.

use super::*;
use crate::nats_session::steering::SteeringFault;
use crate::nats_session::{FollowBoundary, FollowBoundaryOptions, RunTurnOptions};
use std::sync::Arc;

#[tokio::test]
async fn steering_turn_end_race_recovers_within_the_first_call() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steer-race", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let steering = f
        .session("steer-race", "steer-call")
        .await?
        .with_steering_admission();
    // The fault ends the joined turn between the bind and the append: the
    // exact O1 interleaving, driven through one public steering delivery.
    let delivered = steering
        .append_user_content_inner(
            text("steering content"),
            SteeringFault::SteeredTurnEnds.into(),
        )
        .await
        .context("the first steering call must recover the turn-end race in itself")?;
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("steer-call", Some("steer-call"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "steer-call"),
        vec![&text("steering content")],
        "the steered message is delivered exactly once"
    );
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "steer-call")
        .await?
        .context("steered prompt has no admission binding")?;
    assert_eq!(
        binding.invocation_id.as_str(),
        "steer-call",
        "the binding must move off the ended turn onto the fresh reservation"
    );
    // The retry of the same call replays the delivered row identically.
    let replay = steering.append_user_content(text("changed retry")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(
        session.load_durable_entries().await?.len(),
        entries.len(),
        "replay must not grow the log"
    );
    Ok(())
}

#[tokio::test]
async fn existing_row_replay_carries_admitted_invocation_identity() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steer-replay", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let steering = f
        .session("steer-replay", "steer-call")
        .await?
        .with_steering_admission();
    let delivered = steering
        .append_user_content(text("steering content"))
        .await?;
    assert_eq!(
        delivered.execution_id(),
        Some("original"),
        "the first steering delivery names the joined invocation"
    );
    end_invocation_turn(&f, &session, "original").await?;
    // The existing-row retry must replay the row with its admitted invocation
    // identity (O2), not an anonymous receipt.
    let replay = steering.append_user_content(text("changed retry")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(
        replay.execution_id(),
        Some("original"),
        "an existing-row replay must keep the row's admitted invocation"
    );
    // The public boundary-follow identity check accepts the replay receipt;
    // before O2 it refused with "boundary follow requires an admitted
    // invocation identity".
    let boundary = steering
        .follow_admitted_prompt_until_boundary(
            replay,
            Arc::new(harnx_core::event::NullSink),
            FollowBoundaryOptions {
                after_request_seq: None,
                run: RunTurnOptions::default(),
                pending_cancel: None,
            },
        )
        .await
        .context("boundary follow on an existing-row replay must not be refused for identity")?;
    assert!(
        matches!(boundary, FollowBoundary::Terminal(_)),
        "the ended turn is a terminal boundary"
    );
    Ok(())
}
