use super::*;
use harnx_runtime::nats_session::fixed_admission_faults::FixedAdmissionFaults;

#[tokio::test]
async fn paused_old_append_loses_to_close_and_cannot_run_after_context_reuse() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let faults = FixedAdmissionFaults::new();
    let old = open(server.url(), "delayed-owner")
        .await?
        .with_fixed_admission_faults(faults.clone());
    let new = open(server.url(), "delayed-owner").await?;
    let ticket = ticket(&old).await?;
    let pause = faults.pause_next();
    let delayed = tokio::spawn({
        let old = old.clone();
        let ticket = ticket.clone();
        async move { old.append_fixed_prompt(&ticket, text()).await }
    });
    pause.wait_reached().await?;
    let log = log(server.url(), &new).await?;
    assert!(log.load_events_latest_async().await?.is_empty());
    assert!(new
        .metadata_store()
        .active_admission(new.storage_key(), &[])
        .await?
        .is_some());
    assert_eq!(
        new.close_fixed_admission(&ticket).await?,
        Outcome::Closed {
            closure_sequence: 1
        }
    );
    assert_no_pending(&log.load_events_latest_async().await?)?;
    let next = new
        .prepare_fixed_admission(
            "successor-invocation".into(),
            "successor-prompt".into(),
            "successor-close".into(),
        )
        .await?;
    assert_eq!(
        new.append_fixed_prompt(&next, text()).await?,
        Outcome::Admitted { prompt_sequence: 2 }
    );
    drop(pause);
    assert_eq!(
        delayed.await??,
        Outcome::Closed {
            closure_sequence: 1
        }
    );
    let entries = log.load_events_latest_async().await?;
    assert_eq!(entries.len(), 2);
    assert_eq!(
        harnx_core::session_reconstruct::pending_prompt_seq(&entries),
        Some(2)
    );
    let active = new
        .metadata_store()
        .active_admission(new.storage_key(), &entries)
        .await?
        .context("successor active")?;
    assert_eq!(active.invocation_id.as_str(), "successor-invocation");
    Ok(())
}

#[tokio::test]
async fn paused_close_loses_to_prompt_and_resolves_original_execution() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "close-race").await?;
    let faults = FixedAdmissionFaults::new();
    let closer = open(server.url(), "close-race")
        .await?
        .with_fixed_admission_faults(faults.clone());
    let ticket = ticket(&session).await?;
    let pause = faults.pause_next();
    let delayed = tokio::spawn({
        let ticket = ticket.clone();
        async move { closer.close_fixed_admission(&ticket).await }
    });
    pause.wait_reached().await?;
    let admitted = session.append_fixed_prompt(&ticket, text()).await?;
    drop(pause);
    assert_eq!(delayed.await??, admitted);
    let handle = session
        .fixed_prompt_handle(&ticket)
        .await?
        .context("prompt win")?;
    assert_eq!(handle.execution_id(), Some(ticket.invocation_id()));
    assert_eq!(handle.user_msg_seq(), 1);
    assert_eq!(
        log(server.url(), &session)
            .await?
            .load_events_latest_async()
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn dropped_prompt_and_close_ack_resolve_identity_without_extra_append() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    for close in [false, true] {
        let faults = FixedAdmissionFaults::new();
        let session = open(server.url(), if close { "close-ack" } else { "prompt-ack" })
            .await?
            .with_fixed_admission_faults(faults.clone());
        let ticket = ticket(&session).await?;
        faults.lose_next_ack();
        let outcome = if close {
            session.close_fixed_admission(&ticket).await?
        } else {
            session.append_fixed_prompt(&ticket, text()).await?
        };
        let other = open(server.url(), session.session_id()).await?;
        assert_eq!(other.close_fixed_admission(&ticket).await?, outcome);
        assert_eq!(other.append_fixed_prompt(&ticket, text()).await?, outcome);
        assert_eq!(
            log(server.url(), &other)
                .await?
                .load_events_latest_async()
                .await?
                .len(),
            1
        );
    }
    Ok(())
}
