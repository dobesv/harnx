use crate::{common::spawn_nats_server, worker};
use anyhow::{Context, Result};
use harnx_core::{event::NullSink, message::MessageContent, session::SessionLogEntry};
use harnx_runtime::{
    nats_session::fixed_admission::{FixedAdmissionOutcome as Outcome, FixedAdmissionTicket},
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{AdmissionAuthority, InvocationAdmission, SessionInitializer},
    NatsSession, NatsSessionConfig, SessionActivationRoute,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn assert_single_user_and_timing_notice(entries: &[(u64, SessionLogEntry)], timing_msg: &str) {
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::Message { role, content, .. }
                if role.is_user() && !harnx_core::timing_identity::is_timing_notice(&content.to_text())))
            .count(),
        1
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry,
                SessionLogEntry::Message { role, content, .. }
                    if role.is_user() && harnx_core::timing_identity::is_timing_notice(&content.to_text())
            ))
            .count(),
        1,
        "{timing_msg}"
    );
}

async fn open(url: &str, id: &str) -> Result<NatsSession> {
    let client = async_nats::connect(url).await?;
    NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::inline("", Default::default(), Default::default()),
            session_id: Some(id.into()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        async_nats::jetstream::new(client),
        harnx_core::abort::create_abort_signal(),
    )
    .await
    .map(NatsSession::with_external_admission)
}
async fn log(url: &str, session: &NatsSession) -> Result<NatsSessionLog> {
    Ok(NatsSessionLog::new_with_replicas(
        async_nats::jetstream::new(async_nats::connect(url).await?),
        session.storage_key(),
        1,
    ))
}
fn text() -> MessageContent {
    MessageContent::Text("fixed instruction".into())
}
async fn ticket(session: &NatsSession) -> Result<FixedAdmissionTicket> {
    session
        .prepare_fixed_admission(
            "fixed-invocation".into(),
            "fixed-prompt".into(),
            "fixed-close".into(),
        )
        .await
}
async fn reserve(
    session: &NatsSession,
    ticket: &FixedAdmissionTicket,
    log: &NatsSessionLog,
) -> Result<()> {
    let mut intent = InvocationAdmission::new(
        &AdmissionAuthority::External {
            admitted_at: chrono::Utc::now(),
        },
        ticket.invocation_id().into(),
        None,
        None,
    );
    intent.fixed_ticket = Some(ticket.clone());
    intent.prompt_content = Some(text());
    session
        .metadata_store()
        .reserve_admission(
            session.storage_key(),
            &intent,
            &log.load_events_latest_async().await?,
        )
        .await?;
    Ok(())
}
fn assert_no_pending(entries: &[(u64, SessionLogEntry)]) -> Result<()> {
    let effective = harnx_core::session_reconstruct::apply_log_mutations_nats(entries)?;
    assert!(effective.is_empty());
    let session = harnx_runtime::nats_session_log::load_session_from_entries_with_metadata(
        entries,
        "closed-session",
        Default::default(),
    )?;
    assert!(session.messages.is_empty());
    for (_, entry) in entries {
        let payload = harnx_runtime::nats_session_log::serialize_entry(entry)?;
        assert_eq!(
            &harnx_runtime::nats_session_log::deserialize_entry(payload.as_bytes())?,
            entry
        );
    }
    assert_eq!(
        harnx_core::session_reconstruct::pending_prompt_seq(entries),
        None
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn closed_unbound_reservation_reconstructs_idle_and_normal_worker_runs_once() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "closed-reservation").await?;
    let log = log(server.url(), &session).await?;
    let ticket = ticket(&session).await?;
    reserve(&session, &ticket, &log).await?;
    assert!(session
        .metadata_store()
        .active_admission(session.storage_key(), &[])
        .await?
        .is_some());
    let closed = session.close_fixed_admission(&ticket).await?;
    assert_eq!(
        closed,
        Outcome::Closed {
            closure_sequence: 1
        }
    );
    let restored = open(server.url(), "closed-reservation").await?;
    let entries = log.load_events_latest_async().await?;
    assert_no_pending(&entries)?;
    assert!(restored
        .metadata_store()
        .active_admission(restored.storage_key(), &entries)
        .await?
        .is_none());
    assert_eq!(restored.activate_pending_turn().await?, None);
    assert!(restored.fixed_prompt_handle(&ticket).await?.is_none());
    assert_eq!(restored.append_fixed_prompt(&ticket, text()).await?, closed);
    let count = Arc::new(AtomicUsize::new(0));
    let daemon = worker::spawn_worker_daemon_with_call_fn(
        worker::local_nats_runtime_config(server.url()),
        "normal-after-close",
        worker::counting_stub_call_fn(count.clone()),
    )
    .await?;
    let result = restored
        .run_turn("normal instruction", Arc::new(NullSink), None)
        .await?;
    assert_eq!(result.response.as_deref(), Some("done"));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let entries = log.load_events_latest_async().await?;
    assert_single_user_and_timing_notice(
        &entries,
        "one persisted initial notice, not another admitted prompt",
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(_, e)| matches!(e, SessionLogEntry::AdmissionClosed { .. }))
            .count(),
        1
    );
    daemon.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_win_close_returns_same_execution_and_retry_never_duplicates() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "prompt-win").await?;
    let other = open(server.url(), "prompt-win").await?;
    let ticket = ticket(&session).await?;
    let encoded = serde_json::to_vec(&ticket)?;
    let restored: FixedAdmissionTicket = serde_json::from_slice(&encoded)?;
    assert_eq!(restored, ticket);
    let admitted = session.append_fixed_prompt(&ticket, text()).await?;
    assert_eq!(admitted, Outcome::Admitted { prompt_sequence: 1 });
    assert_eq!(other.close_fixed_admission(&restored).await?, admitted);
    let handle = other
        .fixed_prompt_handle(&restored)
        .await?
        .context("same prompt")?;
    assert_eq!(handle.user_msg_id(), ticket.prompt_id());
    assert_eq!(handle.execution_id(), Some(ticket.invocation_id()));
    let count = Arc::new(AtomicUsize::new(0));
    let daemon = worker::spawn_worker_daemon_with_call_fn(
        worker::local_nats_runtime_config(server.url()),
        "fixed-worker",
        worker::counting_stub_call_fn(count.clone()),
    )
    .await?;
    let result = other
        .follow_admitted_prompt(handle, Arc::new(NullSink), None, None, Default::default())
        .await?;
    assert_eq!(result.response.as_deref(), Some("done"));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        session.append_fixed_prompt(&ticket, text()).await?,
        admitted
    );
    assert_eq!(other.close_fixed_admission(&ticket).await?, admitted);
    let entries = log(server.url(), &session)
        .await?
        .load_events_latest_async()
        .await?;
    assert_single_user_and_timing_notice(&entries, "retry must not duplicate the initial notice");
    assert!(!entries.iter().any(|(_, e)| matches!(
        e,
        SessionLogEntry::AdmissionClosed { .. } | SessionLogEntry::Cancel { .. }
    )));
    assert!(session
        .append_fixed_prompt(&ticket, MessageContent::Text("changed".into()))
        .await
        .is_err());
    daemon.abort();
    Ok(())
}

#[tokio::test]
async fn unrelated_advancement_fences_missing_prompt_and_releases_head_without_rebase() -> Result<()>
{
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "unrelated-tail").await?;
    let log = log(server.url(), &session).await?;
    let ticket = ticket(&session).await?;
    reserve(&session, &ticket, &log).await?;
    let unrelated = SessionLogEntry::SubAgentStarted {
        agent: "sibling".into(),
        session_id: "other".into(),
        invocation_id: None,
        tool_call_id: None,
        started_at: None,
    };
    let seq = log.append_event_async(&unrelated).await?;
    assert_eq!(
        session.close_fixed_admission(&ticket).await?,
        Outcome::Fenced { sequence: seq }
    );
    assert_eq!(
        session.append_fixed_prompt(&ticket, text()).await?,
        Outcome::Fenced { sequence: seq }
    );
    assert_eq!(log.load_events_latest_async().await?.len(), 1);
    assert!(session
        .metadata_store()
        .active_admission(
            session.storage_key(),
            &log.load_events_latest_async().await?
        )
        .await?
        .is_none());
    let next = session
        .prepare_fixed_admission(
            "next-invocation".into(),
            "next-prompt".into(),
            "next-close".into(),
        )
        .await?;
    assert_eq!(next.expected_predecessor(), seq);
    assert_eq!(
        session.append_fixed_prompt(&next, text()).await?,
        Outcome::Admitted {
            prompt_sequence: seq + 1
        }
    );
    assert_eq!(
        session.resolve_fixed_admission(&ticket).await?,
        Outcome::Fenced { sequence: seq }
    );
    Ok(())
}

#[tokio::test]
async fn fixed_ticket_identity_and_content_cannot_be_replaced_or_sent_to_other_session(
) -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    let session = open(server.url(), "identity").await?;
    let log = log(server.url(), &session).await?;
    let ticket = ticket(&session).await?;
    reserve(&session, &ticket, &log).await?;
    assert!(session
        .append_fixed_prompt(&ticket, MessageContent::Text("replacement".into()))
        .await
        .is_err());
    let replaced = FixedAdmissionTicket::from_parts(
        session.storage_key().into(),
        ticket.invocation_id().into(),
        "replacement-prompt".into(),
        ticket.closure_id().into(),
        0,
    )?;
    assert!(session
        .append_fixed_prompt(&replaced, text())
        .await
        .is_err());
    assert!(open(server.url(), "other")
        .await?
        .close_fixed_admission(&ticket)
        .await
        .is_err());
    // Ordinary reconstruction/repair must not rebase this missing fixed prompt.
    assert!(session
        .clone()
        .with_admission_id(ticket.invocation_id().into())
        .enqueue_text("repair")
        .await
        .is_err());
    assert!(log.load_events_latest_async().await?.is_empty());
    Ok(())
}

/// Caller-supplied fixed prompt ids must not collide with the reserved
/// runtime-note and timing-notice prefixes: a prompt admitted under one is
/// silently skipped by every executable-prompt check. Both the prepare and
/// the restored-ticket paths must refuse before any durable write.
#[tokio::test]
async fn reserved_prompt_id_prefixes_are_rejected_without_durable_effects() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        anyhow::bail!("nats-server required")
    };
    for (idx, reserved) in ["runtime-note-v1-forged", "timing-v1-forged"]
        .into_iter()
        .enumerate()
    {
        let session = open(server.url(), &format!("reserved-{idx}")).await?;
        let log = log(server.url(), &session).await?;
        assert_reserved_ticket_rejected(&session, "reserved-invocation", reserved).await?;
        assert_restored_ticket_rejected(&session, "reserved-invocation", reserved).await?;
        assert_no_reserved_admission(&session, &log, "reserved-invocation", reserved).await?;
        // The invocation id reaches the transcript too: A2A republish flows
        // it into `with_admission_id`, which appends the prompt under it, so a
        // reserved prefix there must be refused the same way.
        assert_reserved_ticket_rejected(&session, reserved, "normal-prompt").await?;
        assert_restored_ticket_rejected(&session, reserved, "normal-prompt").await?;
        assert_no_reserved_admission(&session, &log, reserved, "normal-prompt").await?;
    }
    Ok(())
}

async fn assert_reserved_ticket_rejected(
    session: &NatsSession,
    invocation: &str,
    prompt: &str,
) -> Result<()> {
    assert!(session
        .prepare_fixed_admission(invocation.into(), prompt.into(), "reserved-close".into())
        .await
        .is_err());
    assert!(FixedAdmissionTicket::from_parts(
        session.storage_key().into(),
        invocation.into(),
        prompt.into(),
        "reserved-close".into(),
        0,
    )
    .is_err());
    Ok(())
}

async fn assert_restored_ticket_rejected(
    session: &NatsSession,
    invocation: &str,
    prompt: &str,
) -> Result<()> {
    // A deserialized ticket bypasses from_parts; every durable entry
    // point must still refuse it.
    let forged: FixedAdmissionTicket = serde_json::from_str(&format!(
        r#"{{"storage_key":{},"invocation_id":{},"prompt_id":{},"closure_id":"reserved-close","expected_predecessor":0}}"#,
        serde_json::to_string(session.storage_key())?,
        serde_json::to_string(invocation)?,
        serde_json::to_string(prompt)?,
    ))?;
    assert!(session.append_fixed_prompt(&forged, text()).await.is_err());
    assert!(session.close_fixed_admission(&forged).await.is_err());
    assert!(session.resolve_fixed_admission(&forged).await.is_err());
    Ok(())
}

async fn assert_no_reserved_admission(
    session: &NatsSession,
    log: &NatsSessionLog,
    invocation: &str,
    prompt: &str,
) -> Result<()> {
    // No log row, no admission reservation, no prompt binding.
    assert!(log.load_events_latest_async().await?.is_empty());
    assert!(session
        .metadata_store()
        .admission(session.storage_key(), invocation)
        .await?
        .is_none());
    assert!(session
        .metadata_store()
        .prompt_admission(session.storage_key(), prompt)
        .await?
        .is_none());
    Ok(())
}

mod edges;
#[cfg(feature = "fault-injection")]
mod faults;
