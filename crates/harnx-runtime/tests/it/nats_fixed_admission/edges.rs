use super::*;

fn unrelated() -> SessionLogEntry {
    SessionLogEntry::SubAgentStarted {
        agent: "sibling".into(),
        session_id: "other".into(),
        invocation_id: None,
        tool_call_id: None,
        started_at: None,
    }
}

#[tokio::test]
async fn nonzero_predecessor_closed_or_fenced_head_allows_next_fixed_admission() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    for close in [true, false] {
        let session = open(
            server.url(),
            if close { "prior-close" } else { "prior-fence" },
        )
        .await?;
        let log = log(server.url(), &session).await?;
        for _ in 0..3 {
            log.append_event_async(&unrelated()).await?;
        }
        let ticket = ticket(&session).await?;
        assert_eq!(ticket.expected_predecessor(), 3);
        reserve(&session, &ticket, &log).await?;
        let outcome = if close {
            session.close_fixed_admission(&ticket).await?
        } else {
            log.append_event_async(&unrelated()).await?;
            session.close_fixed_admission(&ticket).await?
        };
        assert_eq!(
            outcome,
            if close {
                Outcome::Closed {
                    closure_sequence: 4,
                }
            } else {
                Outcome::Fenced { sequence: 4 }
            }
        );
        assert_eq!(session.append_fixed_prompt(&ticket, text()).await?, outcome);
        let restored = open(server.url(), session.session_id()).await?;
        let entries = log.load_events_latest_async().await?;
        assert!(restored
            .metadata_store()
            .active_admission(restored.storage_key(), &entries)
            .await?
            .is_none());
        let next = restored
            .prepare_fixed_admission(
                "next-invocation".into(),
                "next-prompt".into(),
                "next-close".into(),
            )
            .await?;
        assert_eq!(
            restored.append_fixed_prompt(&next, text()).await?,
            Outcome::Admitted { prompt_sequence: 5 }
        );
        assert_eq!(log.load_events_latest_async().await?.len(), 5);
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_prompt_and_close_have_one_durable_winner() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let a = open(server.url(), "concurrent").await?;
    let b = open(server.url(), "concurrent").await?;
    let ticket = ticket(&a).await?;
    let (append, close) = tokio::join!(
        a.append_fixed_prompt(&ticket, text()),
        b.close_fixed_admission(&ticket)
    );
    assert_eq!(append?, close?);
    assert_eq!(
        log(server.url(), &a)
            .await?
            .load_events_latest_async()
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn deleted_or_purged_transcript_is_not_empty_admission_and_never_recreated() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "deleted-log").await?;
    let ticket = ticket(&session).await?;
    session.close_fixed_admission(&ticket).await?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client);
    let name = harnx_runtime::nats_session_log::stream_name_for_session(session.storage_key());
    js.get_stream(&name).await?.purge().await?;
    assert!(session.resolve_fixed_admission(&ticket).await.is_err());
    assert!(session.append_fixed_prompt(&ticket, text()).await.is_err());
    assert!(session.close_fixed_admission(&ticket).await.is_err());
    js.delete_stream(&name).await?;
    assert!(session.append_fixed_prompt(&ticket, text()).await.is_err());
    assert!(session.close_fixed_admission(&ticket).await.is_err());
    assert!(js.get_stream(&name).await.is_err());
    Ok(())
}

#[tokio::test]
async fn closure_is_not_cancel_and_does_not_cover_another_pending_prompt() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "no-global-cancel").await?;
    let log = log(server.url(), &session).await?;
    let admitted = session
        .append_fixed_prompt(&ticket(&session).await?, text())
        .await?;
    assert_eq!(admitted, Outcome::Admitted { prompt_sequence: 1 });
    let other = session
        .prepare_fixed_admission(
            "other-invocation".into(),
            "other-prompt".into(),
            "other-close".into(),
        )
        .await?;
    assert_eq!(
        session.close_fixed_admission(&other).await?,
        Outcome::Closed {
            closure_sequence: 2
        }
    );
    let entries = log.load_events_latest_async().await?;
    assert_eq!(
        harnx_core::session_reconstruct::pending_prompt_seq(&entries),
        Some(1)
    );
    assert!(session
        .metadata_store()
        .active_admission(session.storage_key(), &entries)
        .await?
        .is_some());
    assert!(!entries.iter().any(|(_, e)| matches!(
        e,
        SessionLogEntry::Cancel { .. } | SessionLogEntry::Error { .. }
    )));
    Ok(())
}

#[tokio::test]
async fn editing_out_distinct_fixed_prompt_preserves_raw_invocation_binding() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "edited-root").await?;
    let ticket = ticket(&session).await?;
    let admitted = session.append_fixed_prompt(&ticket, text()).await?;
    let log = log(server.url(), &session).await?;
    log.append_event_async(&SessionLogEntry::EditEntries {
        from: 1,
        to: 1,
        replacements: vec![],
    })
    .await?;
    let entries = log.load_events_latest_async().await?;
    assert_eq!(
        session
            .metadata_store()
            .invocation_prompt_seq(session.storage_key(), ticket.invocation_id(), &entries)
            .await?,
        Some(1)
    );
    assert_eq!(session.close_fixed_admission(&ticket).await?, admitted);
    assert_eq!(
        session.append_fixed_prompt(&ticket, text()).await?,
        admitted
    );
    assert_eq!(log.load_events_latest_async().await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn closure_before_runtime_reservation_blocks_ordinary_replay_of_both_identities() -> Result<()>
{
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "unreserved-close").await?;
    let ticket = ticket(&session).await?;
    session.close_fixed_admission(&ticket).await?;
    assert!(session
        .metadata_store()
        .admission(session.storage_key(), ticket.invocation_id())
        .await?
        .is_none());
    for id in [ticket.invocation_id(), ticket.prompt_id()] {
        assert!(session
            .clone()
            .with_admission_id(id.into())
            .enqueue_text("never replay")
            .await
            .is_err());
    }
    assert_no_pending(
        &log(server.url(), &session)
            .await?
            .load_events_latest_async()
            .await?,
    )?;
    Ok(())
}
