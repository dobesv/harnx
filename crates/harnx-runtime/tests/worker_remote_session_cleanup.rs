mod common;
#[allow(dead_code)]
#[path = "common/worker.rs"]
mod worker;

use anyhow::Result;
use async_nats::jetstream::stream;
use chrono::{TimeZone, Utc};
use futures_util::StreamExt;
use harnx_core::message::{ImageUrl, MessageContent, MessageContentPart};
use harnx_runtime::nats_attachments::{
    delete_session_attachments, externalize_message_attachments, AttachmentLocation,
    SESSION_ATTACHMENTS_BUCKET,
};
use harnx_runtime::nats_session_log::stream_name_for_session;
use harnx_runtime::nats_session_metadata::{
    activity_key, SessionActivity, SessionInitializer, SessionMetadata, SessionMetadataStore,
    SESSION_METADATA_BUCKET,
};
use harnx_runtime::remote_session_cleanup::{
    run_periodic_remote_cleanup_with, CleanupOutcome, PeriodicCleanupOverrides,
    GC_LAST_RUN_EPOCH_HOUR_KEY,
};
use harnx_toolset::ToolRequest;
use harnx_toolset_server::invocation_journal::InvocationJournal;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio_util::task::AbortOnDropHandle;
use worker::{
    counting_stub_call_fn, local_nats_runtime_config, poll_until, require_nats_server,
    spawn_worker_daemon_with_call_fn,
};

fn periodic_overrides(gc_session_id: &str, epoch_hour: u64) -> PeriodicCleanupOverrides<'_> {
    PeriodicCleanupOverrides {
        gc_session_id,
        epoch_hour,
    }
}

struct SeededStaleSession {
    storage_key: String,
    stream_name: String,
    metadata_store: SessionMetadataStore,
}

async fn seed_stale_session(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<SeededStaleSession> {
    let metadata_store = SessionMetadataStore::ensure(jetstream, 1).await?;
    let metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::named("gc-test-agent", Default::default()),
    );
    let storage_key = metadata.storage_key();
    metadata_store.create(&metadata).await?;
    metadata_store
        .kv_store()
        .put(
            activity_key(&storage_key),
            serde_json::to_vec(&SessionActivity {
                first_activation_at: None,
                last_activity_at: Utc.timestamp_opt(1, 0).single().expect("valid timestamp"),
            })?
            .into(),
        )
        .await?;

    let stream_name = stream_name_for_session(&storage_key);
    jetstream
        .create_stream(stream::Config {
            name: stream_name.clone(),
            subjects: vec![format!("sessions.{storage_key}.>")],
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await?;
    jetstream
        .publish(format!("sessions.{storage_key}.event"), "stale".into())
        .await?
        .await?;

    Ok(SeededStaleSession {
        storage_key,
        stream_name,
        metadata_store,
    })
}

async fn session_attachment_exists(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<bool> {
    let store = jetstream
        .get_object_store(SESSION_ATTACHMENTS_BUCKET)
        .await?;
    let prefix = format!("{}/", harnx_core::crypto::sha256(session_id));
    let mut objects = store.list().await?;
    while let Some(info) = objects.next().await {
        if info?.name.starts_with(&prefix) {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn seed_session_journal_and_attachment(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
) -> Result<InvocationJournal> {
    let journal = InvocationJournal::ensure(jetstream, 1).await?;
    journal
        .record(
            &ToolRequest {
                replay: None,
                operation_id: "worker-gc-operation".into(),
                call_id: "worker-gc-call".into(),
                tool: "test_tool".into(),
                args: serde_json::json!({}),
                parent_session_id: Some(storage_key.to_string()),
                tool_call_id: Some("worker-gc-tool-call".into()),
                capabilities: Default::default(),
            },
            ("test_tool", "test-scope", "test-server"),
            1,
        )
        .await?;
    let data_url = format!(
        "data:image/png;base64,{}",
        harnx_core::crypto::base64_encode(b"worker GC attachment")
    );
    let mut attachment = MessageContent::Array(vec![MessageContentPart::ImageUrl {
        image_url: ImageUrl { url: data_url },
    }]);
    externalize_message_attachments(
        AttachmentLocation::new(jetstream, 1, storage_key),
        &mut attachment,
        None,
    )
    .await?;
    Ok(journal)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_collects_stale_session_without_interactive_client() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config = local_nats_runtime_config(server.url());
    config.write().cleanup_remote_sessions_days = Some(1);

    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let seeded = seed_stale_session(&jetstream, "worker-gc-stale").await?;
    let SeededStaleSession {
        storage_key,
        stream_name,
        metadata_store,
    } = seeded;
    let journal = seed_session_journal_and_attachment(&jetstream, &storage_key).await?;

    let daemon = spawn_worker_daemon_with_call_fn(
        Arc::clone(&config),
        "worker-session-gc",
        counting_stub_call_fn(Arc::new(AtomicUsize::new(0))),
    )
    .await?;
    let _daemon = AbortOnDropHandle::new(daemon);

    poll_until(async || {
        let metadata_deleted = metadata_store.get(&storage_key).await?.is_none();
        let stream_deleted = jetstream.get_stream(&stream_name).await.is_err();
        let journal_deleted = journal.records_for_session(&storage_key).await?.is_empty();
        let attachment_deleted = !session_attachment_exists(&jetstream, &storage_key).await?;
        Ok(metadata_deleted && stream_deleted && journal_deleted && attachment_deleted)
    })
    .await?;
    assert!(journal.records_for_session(&storage_key).await?.is_empty());
    assert_eq!(
        delete_session_attachments(&jetstream, &storage_key).await?,
        0,
        "GC should remove the seeded attachment"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_remote_session_cleanup_deduplicates_by_epoch_hour() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config = local_nats_runtime_config(server.url());
    let cleanup_config = config.read().clone();
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let gc_session_id = "periodic-gc-dedup";
    let epoch_hour = 123_456;

    let first = seed_stale_session(&jetstream, "periodic-gc-first").await?;
    let first_outcome = run_periodic_remote_cleanup_with(
        &cleanup_config,
        1,
        "local",
        periodic_overrides(gc_session_id, epoch_hour),
    )
    .await;
    assert!(matches!(first_outcome, CleanupOutcome::Ran(_)));
    poll_until(async || {
        let metadata_deleted = first
            .metadata_store
            .get(&first.storage_key)
            .await?
            .is_none();
        let stream_deleted = jetstream.get_stream(&first.stream_name).await.is_err();
        Ok(metadata_deleted && stream_deleted)
    })
    .await?;

    let second = seed_stale_session(&jetstream, "periodic-gc-second").await?;
    let same_hour = run_periodic_remote_cleanup_with(
        &cleanup_config,
        1,
        "local",
        periodic_overrides(gc_session_id, epoch_hour),
    )
    .await;
    assert_eq!(same_hour, CleanupOutcome::Skipped);
    assert!(second
        .metadata_store
        .get(&second.storage_key)
        .await?
        .is_some());
    assert!(jetstream.get_stream(&second.stream_name).await.is_ok());

    let next_hour = run_periodic_remote_cleanup_with(
        &cleanup_config,
        1,
        "local",
        periodic_overrides(gc_session_id, epoch_hour + 1),
    )
    .await;
    assert!(matches!(next_hour, CleanupOutcome::Ran(_)));
    poll_until(async || {
        let metadata_deleted = second
            .metadata_store
            .get(&second.storage_key)
            .await?
            .is_none();
        let stream_deleted = jetstream.get_stream(&second.stream_name).await.is_err();
        Ok(metadata_deleted && stream_deleted)
    })
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_remote_session_cleanup_scan_failure_does_not_write_marker() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config = local_nats_runtime_config(server.url());
    let cleanup_config = config.read().clone();
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    jetstream
        .create_stream(stream::Config {
            name: format!("KV_{SESSION_METADATA_BUCKET}"),
            subjects: vec!["broken.session.metadata".into()],
            ..Default::default()
        })
        .await?;

    let outcome = run_periodic_remote_cleanup_with(
        &cleanup_config,
        1,
        "local",
        periodic_overrides("periodic-gc-scan-failure", 200_000),
    )
    .await;
    assert_eq!(outcome, CleanupOutcome::Failed);
    let lease_store = cleanup_config
        .nats_kv_bucket("local", "harnx_leases")
        .await?;
    assert!(lease_store.get(GC_LAST_RUN_EPOCH_HOUR_KEY).await?.is_none());

    let retry = run_periodic_remote_cleanup_with(
        &cleanup_config,
        1,
        "local",
        periodic_overrides("periodic-gc-scan-failure", 200_000),
    )
    .await;
    assert_eq!(retry, CleanupOutcome::Failed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_remote_session_cleanup_marker_read_failure_releases_lease() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config = local_nats_runtime_config(server.url());
    let cleanup_config = config.read().clone();
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let gc_session_id = "periodic-gc-marker-read-failure";

    let bootstrap = run_periodic_remote_cleanup_with(
        &cleanup_config,
        1,
        "local",
        periodic_overrides(gc_session_id, 300_000),
    )
    .await;
    assert!(matches!(bootstrap, CleanupOutcome::Ran(_)));
    let lease_store = cleanup_config
        .nats_kv_bucket("local", "harnx_leases")
        .await?;
    // A write-only failure would require racing bucket deletion between scan and marker update.
    lease_store
        .put(GC_LAST_RUN_EPOCH_HOUR_KEY, "not-an-hour".into())
        .await?;
    let stale = seed_stale_session(&jetstream, "marker-read-failure-stale").await?;

    let failed = run_periodic_remote_cleanup_with(
        &cleanup_config,
        1,
        "local",
        periodic_overrides(gc_session_id, 300_001),
    )
    .await;
    assert_eq!(failed, CleanupOutcome::Failed);
    assert!(stale
        .metadata_store
        .get(&stale.storage_key)
        .await?
        .is_some());
    assert!(jetstream.get_stream(&stale.stream_name).await.is_ok());

    let retry = run_periodic_remote_cleanup_with(
        &cleanup_config,
        1,
        "local",
        periodic_overrides(gc_session_id, 300_001),
    )
    .await;
    assert_eq!(retry, CleanupOutcome::Failed);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_remote_session_cleanup_reports_not_leader_for_election_loser() -> Result<()> {
    let Some(server) = require_nats_server().await? else {
        return Ok(());
    };
    let config = local_nats_runtime_config(server.url());
    let cleanup_config = config.read().clone();
    let client = async_nats::connect(server.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let _stale = seed_stale_session(&jetstream, "periodic-gc-election").await?;
    let gc_session_id = "periodic-gc-election";
    let epoch_hour = 400_000;

    let (first, second) = tokio::join!(
        run_periodic_remote_cleanup_with(
            &cleanup_config,
            1,
            "local",
            periodic_overrides(gc_session_id, epoch_hour),
        ),
        run_periodic_remote_cleanup_with(
            &cleanup_config,
            1,
            "local",
            periodic_overrides(gc_session_id, epoch_hour),
        )
    );

    assert!(matches!(
        (&first, &second),
        (CleanupOutcome::Ran(_), CleanupOutcome::NotLeader)
            | (CleanupOutcome::NotLeader, CleanupOutcome::Ran(_))
            | (CleanupOutcome::Skipped, CleanupOutcome::NotLeader)
            | (CleanupOutcome::NotLeader, CleanupOutcome::Skipped)
    ));
    Ok(())
}
