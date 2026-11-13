//! Issue #2672 review round 2 (C1/C2/C10): the redirect half-states and
//! the steering replay of a delivered prompt.

use super::*;

/// A live non-fixed admission head the way a running worker leaves one: the
/// head is active and rowless, so it can never be classified terminal.
async fn live_admission_head(session: &NatsSession, f: &Fixture, invocation: &str) -> Result<()> {
    let authority = AdmissionAuthority::Inherited {
        parent: f.parent.clone(),
        edge: InvocationEdgeKind::Delegation,
        admitted_at: f.parent.admitted_at,
    };
    let mut intent = InvocationAdmission::new(&authority, invocation.into(), None, None);
    intent.parent_storage_key = Some("trusted-parent".into());
    intent.prompt_content = Some(text("live head work"));
    let entries = session.load_durable_entries().await?;
    session
        .metadata_store
        .reserve_admission(session.storage_key(), &intent, &entries)
        .await?;
    assert_eq!(
        session
            .metadata_store
            .admission_head(session.storage_key())
            .await?,
        Some(invocation.to_owned())
    );
    Ok(())
}

/// C10: the crash window between a void-join record retarget and its binding
/// move — the record already names the live target, the binding still names
/// the dead invocation. A retry of the same call must repair the binding and
/// deliver into the record's target.
#[tokio::test]
async fn retargeted_record_with_stale_binding_repairs_and_delivers() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("stale-binding", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    end_invocation_turn(&f, &session, "original").await?;
    live_admission_head(&session, &f, "live-head").await?;
    let parent = foreign_parent().await?;
    let caller = joining_caller(&f, "stale-binding", "stale-call", &parent).await?;
    session
        .metadata_store
        .create_joined_dispatch(
            session.storage_key(),
            "stale-call",
            &void_join_record(&parent, "stale content", "live-head"),
        )
        .await?;
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "stale-call", "original")
        .await?;
    let delivered = caller
        .append_dispatch_content(text("changed retry"))
        .await
        .context("a record-new/binding-stale join must repair and deliver")?;
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("stale-call", Some("live-head"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "stale-call"),
        vec![&text("stale content")],
        "the record's content stays authoritative across the repair"
    );
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "stale-call")
        .await?
        .context("repaired prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "live-head");
    let replay = caller.append_dispatch_content(text("again")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

/// C1/C10: a void join whose redirect lands on a live non-fixed head. The
/// same call must deliver into that head within one public call, with no
/// spurious `SessionBusy` from the redirected state.
#[tokio::test]
async fn void_join_redirect_lands_on_live_head_without_busy() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("live-redirect", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let parent = foreign_parent().await?;
    let caller = joining_caller(&f, "live-redirect", "redirect-call", &parent).await?;
    session
        .metadata_store
        .create_joined_dispatch(
            session.storage_key(),
            "redirect-call",
            &void_join_record(&parent, "redirect content", "original"),
        )
        .await?;
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "redirect-call", "original")
        .await?;
    end_invocation_turn(&f, &session, "original").await?;
    live_admission_head(&session, &f, "live-head").await?;
    let delivered = caller
        .append_dispatch_content(text("changed retry"))
        .await
        .context("a void join must redirect onto the live head without busy")?;
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("redirect-call", Some("live-head"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "redirect-call"),
        vec![&text("redirect content")]
    );
    let record = session
        .metadata_store
        .joined_dispatch(session.storage_key(), "redirect-call")
        .await?
        .context("redirected join record missing")?;
    assert_eq!(record.invocation_id, "live-head");
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "redirect-call")
        .await?
        .context("redirected prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "live-head");
    let replay = caller.append_dispatch_content(text("again")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

/// C2: a double retarget — the record moved in one crash window, its new
/// target died undelivered too, and the binding still names the first dead
/// generation. The retry must converge, not fail with a binding conflict.
#[tokio::test]
async fn double_retarget_with_older_stale_binding_converges() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("double-retarget", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    end_invocation_turn(&f, &session, "original").await?;
    // The first redirect's target, which also died undelivered.
    let second = f.session("double-retarget", "second").await?;
    second.append_dispatch_content(text("second work")).await?;
    end_invocation_turn(&f, &session, "second").await?;
    let parent = foreign_parent().await?;
    let caller = joining_caller(&f, "double-retarget", "lag-call", &parent).await?;
    session
        .metadata_store
        .create_joined_dispatch(
            session.storage_key(),
            "lag-call",
            &void_join_record(&parent, "lag content", "second"),
        )
        .await?;
    session
        .metadata_store
        .bind_prompt_admission(session.storage_key(), "lag-call", "original")
        .await?;
    let delivered = caller
        .append_dispatch_content(text("changed retry"))
        .await
        .context("a double retarget must converge, not fail with a binding conflict")?;
    assert_eq!(
        (delivered.user_msg_id(), delivered.execution_id()),
        ("lag-call", Some("lag-call"))
    );
    let entries = session.load_durable_entries().await?;
    assert_eq!(
        user_messages(&entries, "lag-call"),
        vec![&text("lag content")],
        "the record's content stays authoritative across both redirects"
    );
    let record = session
        .metadata_store
        .joined_dispatch(session.storage_key(), "lag-call")
        .await?
        .context("retargeted join record missing")?;
    assert_eq!(record.invocation_id, "lag-call");
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "lag-call")
        .await?
        .context("converged prompt has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "lag-call");
    let replay = caller.append_dispatch_content(text("again")).await?;
    assert_eq!(replay, delivered);
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}

/// C10: a delivered steering prompt whose turn then ends must replay stably —
/// no new reservation, no binding move, no log growth. The replay returns the
/// delivering invocation before any recovery decision, like dispatch.
#[tokio::test]
async fn delivered_steering_replays_stably_after_turn_end() -> Result<()> {
    let f = Fixture::new().await?;
    let session = f.session("steer-stable", "original").await?;
    session
        .append_dispatch_content(text("original work"))
        .await?;
    let steering = f
        .session("steer-stable", "steer-call")
        .await?
        .with_steering_admission();
    let enqueued = steering.enqueue_text("delivered steering").await?;
    end_invocation_turn(&f, &session, "original").await?;
    let before = session.load_durable_entries().await?;
    // The public replay converges on the committed row.
    let replay = steering.enqueue_text("changed replay").await?;
    assert_eq!(replay.user_msg_seq, enqueued.user_msg_seq);
    let entries = session.load_durable_entries().await?;
    assert_eq!(entries.len(), before.len(), "replay must not grow the log");
    assert_eq!(
        user_messages(&entries, "steer-call"),
        vec![&text("delivered steering")]
    );
    // The admission replay itself returns the delivering invocation before
    // any recovery decision (C10; matches dispatch's original-row return).
    let invocation = steering
        .admit_prompt("steer-call", true, Some(&text("changed replay")))
        .await
        .context("delivered steering replay must return the delivering invocation")?;
    assert_eq!(invocation, "original");
    let binding = session
        .metadata_store
        .prompt_admission(session.storage_key(), "steer-call")
        .await?
        .context("delivered steering has no admission binding")?;
    assert_eq!(binding.invocation_id.as_str(), "original");
    assert_eq!(session.load_durable_entries().await?.len(), entries.len());
    Ok(())
}
