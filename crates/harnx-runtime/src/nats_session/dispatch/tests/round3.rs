//! CodeRabbit round on PR #2694: the dispatch-path races I1, I2 and I3, each
//! reproduced deterministically at the exact interleaving the review
//! describes.

use super::*;

#[tokio::test]
async fn own_live_invocation_is_never_joined() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("self-join", "race-call").await?;
    // R1 reserves this prompt's own invocation and pauses before the append.
    let error = session
        .append_dispatch_content_inner(text("frozen content"), DispatchFault::AfterAdmission.into())
        .await
        .expect_err("injected dispatch failure");
    assert!(format!("{error:#}").contains("injected dispatch failure"));
    // R2 holds the same call id with a stale owns_head=false read and a
    // different raw retry text; its fresh active lookup sees its own live
    // reservation P (I1).
    let authority = AdmissionAuthority::Inherited {
        parent: f.parent.clone(),
        edge: InvocationEdgeKind::Delegation,
        admitted_at: f.parent.admitted_at,
    };
    let mut intent = InvocationAdmission::new(&authority, "race-call".into(), None, None);
    intent.parent_storage_key = Some("trusted-parent".into());
    intent.prompt_content = Some(text("changed retry"));
    let outcome = session
        .join_active_dispatch(&intent, &DispatchAttempt::default())
        .await?;
    assert!(
        matches!(outcome, ReplayOutcome::ProceedToReserve),
        "a prompt must never join its own live invocation"
    );
    assert!(session
        .metadata_store
        .joined_dispatch(session.storage_key(), "race-call")
        .await?
        .is_none());
    // The reservation stays sole owner: the public retry delivers the frozen
    // content, one row, and replays identically.
    let delivered = session
        .append_dispatch_content(text("changed retry"))
        .await
        .context("reservation replay must deliver the frozen content")?;
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("race-call", Some("race-call"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "race-call"),
        vec![&text("frozen content")],
        "the frozen reservation content wins over the retry's text"
    );
    let replay = session.append_dispatch_content(text("again")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

#[tokio::test]
async fn stale_captured_join_receipt_reports_current_binding() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("stale-receipt", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let parent = foreign_parent().await?;
    let caller = joining_caller(&f, "stale-receipt", "join-call", &parent).await?;
    // R1 captures the joined record J->original, then pauses before the
    // fresh log read that builds its receipt.
    let captured = void_join_record(&parent, "joined content", "original");
    session
        .metadata_store
        .create_joined_dispatch(session.storage_key(), "join-call", &captured)
        .await?;
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "join-call", "original")
        .await?;
    end_invocation_turn(&f, &session, "original").await?;
    // A concurrent retry of the same call proves the void join, redirects the
    // record and binding to a fresh reservation, and delivers J.
    let delivered = caller
        .append_dispatch_content(text("changed retry"))
        .await
        .context("concurrent retry must redirect and deliver")?;
    assert_eq!(delivered.execution_id(), Some("join-call"));
    // R1 resumes with its captured record: the receipt must report the
    // CURRENT binding, not the dead captured invocation (I2).
    let authority = AdmissionAuthority::Inherited {
        parent: parent.clone(),
        edge: InvocationEdgeKind::Delegation,
        admitted_at: parent.admitted_at,
    };
    let mut intent = InvocationAdmission::new(&authority, "join-call".into(), None, None);
    intent.parent_storage_key = Some("joiner-parent".into());
    intent.prompt_content = Some(text("joined content"));
    let outcome = caller
        .finish_joined_dispatch(&intent, &captured, &DispatchAttempt::default())
        .await?;
    let receipt = match outcome {
        ReplayOutcome::Delivered(receipt) => receipt,
        _ => anyhow::bail!("stale captured join must replay the delivered row"),
    };
    assert_eq!(
        receipt.execution_id(),
        Some("join-call"),
        "the receipt reports the current binding, not the captured invocation"
    );
    // Activation accepts the receipt: no "dispatch execution binding mismatch".
    assert_eq!(
        caller.activate_dispatch_prompt(&receipt).await?,
        PromptActivation::Published
    );
    assert_eq!(receipt, delivered);
    Ok(())
}

#[tokio::test]
async fn leftover_candidate_content_does_not_block_void_recovery() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("leftover-void", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let parent = foreign_parent().await?;
    let caller = joining_caller(&f, "leftover-void", "candidate", &parent).await?;
    // A busy reserve against the live turn A leaves candidate P/X behind.
    let entries = session.load_durable_entries().await?;
    let authority = AdmissionAuthority::Inherited {
        parent: parent.clone(),
        edge: InvocationEdgeKind::Delegation,
        admitted_at: parent.admitted_at,
    };
    let mut candidate = InvocationAdmission::new(&authority, "candidate".into(), None, None);
    candidate.parent_storage_key = Some("joiner-parent".into());
    candidate.prompt_content = Some(text("candidate content"));
    let error = session
        .metadata_store
        .reserve_admission(session.storage_key(), &candidate, &entries)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<SessionBusy>().is_some());
    // The join P->A/Y is written, the append is lost, and A ends undelivered.
    session
        .metadata_store
        .create_joined_dispatch(
            session.storage_key(),
            "candidate",
            &void_join_record(&parent, "joined content", "original"),
        )
        .await?;
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "candidate", "original")
        .await?;
    end_invocation_turn(&f, &session, "original").await?;
    // The FIRST recovery must supersede the stale candidate content and
    // deliver the joined record's content once (D5, I3).
    let delivered = caller
        .append_dispatch_content(text("changed retry"))
        .await
        .context("first void recovery must deliver despite the leftover candidate")?;
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("candidate", Some("candidate"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "candidate"),
        vec![&text("joined content")],
        "the joined record's content is authoritative (D5)"
    );
    // The candidate admission carries the joined content now, so every later
    // replay agrees with the delivered row.
    let admission = session
        .metadata_store
        .admission(session.storage_key(), "candidate")
        .await?
        .context("candidate admission missing")?;
    assert_eq!(admission.prompt_content, Some(text("joined content")));
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "candidate")
        .await?
        .context("recovered prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "candidate");
    let replay = caller.append_dispatch_content(text("again")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}
