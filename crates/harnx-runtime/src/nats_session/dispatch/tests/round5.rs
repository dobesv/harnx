//! CodeRabbit round on PR #2694 against HEAD ed8cd0b02: A (a stale frozen
//! candidate wins the admission head after the void recovery recontented it)
//! and B (the late-steering recovery re-delivers instead of replaying a row
//! a competing retry committed).

use super::*;
use crate::nats_session::steering::SteeringFault;

#[tokio::test]
async fn stale_frozen_candidate_cannot_win_the_head_after_recontent() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("race-win", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let (candidate, frozen) = freeze_candidate_before_recontent(&f, &session).await?;
    // The joined record P->A/Y turns terminal undelivered, and its void
    // recovery recontents the candidate to the record's authoritative Y.
    let parent = foreign_parent().await?;
    session
        .metadata_store
        .create_joined_dispatch(
            session.storage_key(),
            "race-call",
            &void_join_record(&parent, "joined content", "original"),
        )
        .await?;
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "race-call", "original")
        .await?;
    end_invocation_turn(&f, &session, "original").await?;
    session
        .metadata_store
        .recontent_candidate_admission(session.storage_key(), "race-call", &text("joined content"))
        .await?;
    // The paused resumer tries to win the head with the stale frozen X: the
    // head CAS alone does not see the recontent, so the win must be fenced
    // (CodeRabbit A) instead of delivering superseded content.
    let entries = session.load_durable_entries().await?;
    let stale = session
        .metadata_store
        .win_admission_head(session.storage_key(), &frozen, &entries)
        .await;
    let error = stale.expect_err("a stale frozen candidate must not win the head after recontent");
    assert!(
        error.downcast_ref::<SessionBusy>().is_some(),
        "the fence must use the typed busy refusal so the caller re-decides: {error:#}"
    );
    assert_eq!(
        session
            .metadata_store
            .admission_head(session.storage_key())
            .await?,
        Some("original".into()),
        "the refused win must leave the head untouched"
    );
    assert_recovery_wins_with_joined_content(&session, &candidate, &entries).await?;
    Ok(())
}

async fn assert_recovery_wins_with_joined_content(
    session: &NatsSession,
    candidate: &InvocationAdmission,
    entries: &[(u64, SessionLogEntry)],
) -> Result<()> {
    // The real recovery still wins with the authoritative joined content.
    let mut recovered = candidate.clone();
    recovered.prompt_content = Some(text("joined content"));
    let reserved = session
        .metadata_store
        .reserve_admission(session.storage_key(), &recovered, entries)
        .await
        .context("the void recovery must still win the head")?;
    assert_eq!(reserved.prompt_content, Some(text("joined content")));
    assert_eq!(
        session
            .metadata_store
            .admission_head(session.storage_key())
            .await?,
        Some("race-call".into())
    );
    Ok(())
}

async fn freeze_candidate_before_recontent(
    f: &Fixture,
    session: &NatsSession,
) -> Result<(InvocationAdmission, InvocationAdmission)> {
    // A busy reserve leaves candidate P/X while its caller holds the frozen X snapshot.
    let entries = session.load_durable_entries().await?;
    let authority = AdmissionAuthority::Inherited {
        parent: f.parent.clone(),
        edge: InvocationEdgeKind::Delegation,
        admitted_at: f.parent.admitted_at,
    };
    let mut candidate = InvocationAdmission::new(&authority, "race-call".into(), None, None);
    candidate.parent_storage_key = Some("trusted-parent".into());
    candidate.prompt_content = Some(text("candidate content"));
    let busy = session
        .metadata_store
        .reserve_admission(session.storage_key(), &candidate, &entries)
        .await
        .unwrap_err();
    assert!(busy.downcast_ref::<SessionBusy>().is_some());
    let frozen = session
        .metadata_store
        .admission(session.storage_key(), "race-call")
        .await?
        .context("busy reserve must leave the candidate intent")?;
    Ok((candidate, frozen))
}

#[tokio::test]
async fn late_steering_recovery_replays_a_row_committed_by_a_competing_retry() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steer-compete", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let steering = f
        .session("steer-compete", "steer-call")
        .await?
        .with_steering_admission();
    // The fault makes a competing retry deliver this prompt's committed row
    // and end the joined turn between this call's bind and append
    // (CodeRabbit B): the first append refuses late steering, and the
    // recovery must replay the committed row instead of re-delivering into
    // the same refusal.
    let delivered = steering
        .append_user_content_inner(
            text("steering content"),
            SteeringFault::SteeredCompetingDeliveryEndsTurn.into(),
        )
        .await
        .context("recovery must replay the competing retry's committed row")?;
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("steer-call", Some("original")),
        "the committed row's receipt names the prompt's binding"
    );
    assert_eq!(
        delivered.user_msg_seq(),
        entries
            .iter()
            .find(|(_, entry)| matches!(entry, SessionLogEntry::Message { id: Some(id), .. } if id == "steer-call"))
            .context("committed row")?
            .0,
        "the receipt is the committed row, not a new append"
    );
    assert_eq!(
        user_messages(&entries, "steer-call"),
        vec![&text("competing steering")],
        "exactly one row, the competing retry's content"
    );
    // A later same-id call replays the committed row identically.
    let replay = steering.append_user_content(text("changed retry")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

async fn coordinated_steer(
    session: &NatsSession,
    content: &str,
) -> Result<crate::nats_session::AppendedPrompt> {
    let input = crate::config::Input::new(
        content.to_owned(),
        (content.to_owned(), vec![]),
        Default::default(),
    );
    session
        .clone()
        .with_external_admission()
        .with_admission_id("steer-m".into())
        .admit_input_steered(&input, "original")
        .await
}

/// The coordinated steering path (A2A issue #2774) replays a committed
/// same-id row only when the retry's content matches it: the frontend's
/// changed-parts check lives in its dedupe mapping, which is written after
/// the row, so a retry that arrives while the mapping is absent (a failed
/// write, or the documented append-before-mapping crash window) must not be
/// answered with — or recorded against — a row the caller never sent.
#[tokio::test]
async fn coordinated_steering_replay_refuses_a_changed_same_id_retry() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steer-content", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let delivered = coordinated_steer(&session, "first parts").await?;
    // A same-content retry replays the committed row.
    let replay = coordinated_steer(&session, "first parts").await?;
    assert_eq!(replay.user_msg_seq(), delivered.user_msg_seq());
    // A changed retry under the same admission id is refused, not replayed.
    let changed = coordinated_steer(&session, "changed parts")
        .await
        .unwrap_err();
    assert!(
        changed
            .downcast_ref::<crate::nats_session::SteeredContentMismatch>()
            .is_some(),
        "a changed same-id retry must surface the typed content mismatch: {changed:#}"
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "steer-m"),
        vec![&text("first parts")],
        "no second row for the changed retry"
    );
    Ok(())
}
