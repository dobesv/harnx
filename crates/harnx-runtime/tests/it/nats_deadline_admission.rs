//! Real broker admission/fencing tests. No worker enforcement claims here.
use crate::common::spawn_nats_server;
use anyhow::{Context, Result};
use harnx_core::{event::NullSink, message::MessageContent, session::SessionLogEntry};
use harnx_runtime::{
    nats_session::{interrupt::interrupt_invocation, InterruptOutcome, InterruptRequest},
    nats_session_log::{FencedAppend, NatsSessionLog},
    nats_session_metadata::{AdmissionAuthority, InvocationAdmission, SessionInitializer},
    NatsSession, NatsSessionConfig, SessionActivationRoute,
};
use std::sync::Arc;

async fn open(url: &str, id: &str) -> Result<NatsSession> {
    let client = async_nats::connect(url).await?;
    NatsSession::new(
        NatsSessionConfig {
            cluster: "local".into(),
            initializer: SessionInitializer::named("target", Default::default()),
            session_id: Some(id.into()),
            activation_route: SessionActivationRoute::ClusterShared,
        },
        client.clone(),
        async_nats::jetstream::new(client),
        harnx_core::abort::create_abort_signal(),
    )
    .await
}

#[tokio::test]
async fn missing_authority_fails_closed_busy_conflicts_and_steering_reuses_admission() -> Result<()>
{
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let session = open(server.url(), "admission-busy").await?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let log = NatsSessionLog::new(js, session.storage_key());
    assert!(session
        .run_turn("contextless", Arc::new(NullSink), None)
        .await
        .is_err());
    assert!(log.load_events_async().await?.is_empty());
    let external = session
        .clone()
        .with_external_admission()
        .with_admission_timeout(Some(30));
    let first = external.enqueue_text("first instruction").await?;
    let entries = log.load_events_async().await?;
    let first_id = entries
        .iter()
        .find_map(|(_, entry)| match entry {
            SessionLogEntry::Message {
                id: Some(id), role, ..
            } if role.is_user() => Some(id),
            _ => None,
        })
        .context("root prompt")?;
    let original = session
        .metadata_store()
        .prompt_admission(session.storage_key(), first_id)
        .await?
        .context("original admission")?;
    assert!(session
        .clone()
        .with_external_admission()
        .run_turn("distinct independent run", Arc::new(NullSink), None)
        .await
        .unwrap_err()
        .to_string()
        .contains("session busy"));
    let steering = session
        .clone()
        .with_external_admission()
        .enqueue_text("interactive steering")
        .await?;
    let entries = log.load_events_async().await?;
    let steering_id = entries
        .iter()
        .find_map(|(seq, entry)| match entry {
            SessionLogEntry::Message { id: Some(id), .. } if *seq == steering.user_msg_seq() => {
                Some(id)
            }
            _ => None,
        })
        .unwrap();
    let inherited = session
        .metadata_store()
        .prompt_admission(session.storage_key(), steering_id)
        .await?
        .unwrap();
    assert_eq!(original, inherited);
    assert!(steering.user_msg_seq() > first.user_msg_seq());
    Ok(())
}

#[tokio::test]
async fn pre_append_crash_is_repaired_without_renewing_admission_time() -> Result<()> {
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let session = open(server.url(), "admission-repair").await?;
    let js = async_nats::jetstream::new(async_nats::connect(server.url()).await?);
    let log = NatsSessionLog::new(js, session.storage_key());
    let original_time = chrono::Utc::now() - chrono::Duration::seconds(20);
    let mut intent = InvocationAdmission::new(
        &AdmissionAuthority::External {
            admitted_at: original_time,
        },
        "stable-admission".into(),
        Some(10),
        None,
    );
    intent.prompt_content = Some(MessageContent::Text(
        "original persisted instruction".into(),
    ));
    session
        .metadata_store()
        .reserve_admission(session.storage_key(), &intent, &[])
        .await?;
    assert!(log.load_events_async().await?.is_empty());
    assert!(session.republish_pending_activation().await?);
    let entries = log.load_events_async().await?;
    assert_eq!(
        entries
            .iter()
            .filter(|(_, entry)| matches!(entry, SessionLogEntry::Message { .. }))
            .count(),
        1
    );
    let restored = session
        .metadata_store()
        .prompt_admission(session.storage_key(), "stable-admission")
        .await?
        .unwrap();
    assert_eq!(restored.admitted_at, original_time);
    assert_eq!(restored, intent);
    assert!(session.republish_pending_activation().await?);
    assert_eq!(log.load_events_async().await?.len(), entries.len());
    Ok(())
}

#[tokio::test]
async fn completion_expiry_race_has_one_terminal_and_late_stop_cannot_target_new_run() -> Result<()>
{
    harnx_core::require_nextest();
    let Some(server) = spawn_nats_server().await? else {
        return Ok(());
    };
    let session = open(server.url(), "admission-race").await?;
    let client = async_nats::connect(server.url()).await?;
    let js = async_nats::jetstream::new(client.clone());
    let log = NatsSessionLog::new(js.clone(), session.storage_key());
    for index in 0..20 {
        let prompt = session
            .clone()
            .with_external_admission()
            .enqueue_text(&format!("instruction {index}"))
            .await?;
        let seq = prompt.user_msg_seq();
        let end = SessionLogEntry::TurnEnd {
            through_seq: seq,
            fence_token: 0,
            usage: None,
            timestamp: None,
        };
        let request = InterruptRequest {
            session_id: session.storage_key().into(),
            cluster: "local".into(),
            replicas: 1,
            cancellation_id: format!("expiry-{index}"),
            requested_by: "timer".into(),
            reason: "expired".into(),
        };
        let completion_id = format!("completion-{index}");
        let (completed, stopped) = tokio::join!(
            log.append_fenced(&end, seq, &completion_id),
            interrupt_invocation(
                &js,
                &client,
                &SessionActivationRoute::ClusterShared,
                request,
                seq
            )
        );
        let completed = completed?;
        let stopped = stopped?;
        assert!(matches!(
            (completed, &stopped),
            (FencedAppend::Appended(_), InterruptOutcome::Idle)
                | (
                    FencedAppend::Conflict { .. },
                    InterruptOutcome::Accepted { .. }
                )
        ));
        let entries = log.load_events_async().await?;
        assert_eq!(
            entries
                .iter()
                .filter(|(row, entry)| *row > seq
                    && matches!(
                        entry,
                        SessionLogEntry::Cancel { .. }
                            | SessionLogEntry::TurnEnd { .. }
                            | SessionLogEntry::Error { .. }
                    ))
                .count(),
            1
        );
    }
    Ok(())
}
