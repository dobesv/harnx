use crate::worker;

use anyhow::Result;
use async_nats::jetstream::stream;
use harnx_runtime::nats_session_log::{stream_name_for_session, subject_for_session};
use harnx_runtime::nats_session_metadata::{
    SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use harnx_runtime::remote_session_cleanup::{
    run_periodic_remote_cleanup_with, CleanupOutcome, PeriodicCleanupOverrides, RemoteCleanupStats,
};
use harnx_runtime::session_stream_orphans::{
    cleanup_orphan_session_streams, delete_orphan_stream_guarded, list_orphan_session_streams,
    GuardedOrphanDelete, OrphanStreamCleanupOptions, OrphanStreamCleanupStats,
    OrphanStreamCleanupWindow, OrphanStreamInventory, OrphanStreamReport,
};
use worker::{local_nats_runtime_config, require_nats_server};

const AGENT: &str = "orphan-stream-agent";
const GRACE_SECS: u64 = 24 * 60 * 60;

fn storage_key(session_id: &str) -> String {
    harnx_core::session_identity::session_key(Some(AGENT), session_id)
}

fn wall_clock_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_secs()
}

/// Ensure the session metadata bucket exists. Orphan reconciliation
/// deliberately refuses to run without it: metadata presence is a hard
/// deletion guard, and the metadata GC also skips clusters without the
/// bucket.
async fn ensure_metadata_bucket(jetstream: &async_nats::jetstream::Context) -> Result<()> {
    SessionMetadataStore::ensure(jetstream, 1).await.map(|_| ())
}

/// A `SESSION_` stream for `storage_key` with `messages` messages and no
/// session metadata — the state the periodic GC can never see today.
async fn seed_orphan_stream(
    jetstream: &async_nats::jetstream::Context,
    storage_key: &str,
    messages: u64,
) -> Result<String> {
    let stream_name = stream_name_for_session(storage_key);
    let subject = subject_for_session(storage_key);
    jetstream
        .create_stream(stream::Config {
            name: stream_name.clone(),
            subjects: vec![subject.clone()],
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await?;
    for _ in 0..messages {
        jetstream
            .publish(subject.clone(), "orphan".into())
            .await?
            .await?;
    }
    Ok(stream_name)
}

/// Write canonical session metadata for a session whose storage key the test
/// already derived with [`storage_key`].
async fn seed_session_metadata(
    jetstream: &async_nats::jetstream::Context,
    session_id: &str,
) -> Result<SessionMetadataStore> {
    let store = SessionMetadataStore::ensure(jetstream, 1).await?;
    let metadata = SessionMetadata::new(
        session_id,
        SessionInitializer::named(AGENT, Default::default()),
    );
    assert_eq!(metadata.storage_key(), storage_key(session_id));
    store.create(&metadata).await?;
    Ok(store)
}

async fn stream_exists(jetstream: &async_nats::jetstream::Context, storage_key: &str) -> bool {
    jetstream
        .get_stream(&stream_name_for_session(storage_key))
        .await
        .is_ok()
}

struct OrphanFixture {
    config: harnx_runtime::config::Config,
    jetstream: async_nats::jetstream::Context,
    _server: crate::common::NatsServerHandle,
}

impl OrphanFixture {
    async fn start() -> Result<Option<Self>> {
        let Some(server) = require_nats_server().await? else {
            return Ok(None);
        };
        let config = local_nats_runtime_config(server.url()).read().clone();
        let client = async_nats::connect(server.url()).await?;
        let jetstream = async_nats::jetstream::new(client);
        ensure_metadata_bucket(&jetstream).await?;
        Ok(Some(Self {
            _server: server,
            config,
            jetstream,
        }))
    }

    async fn cleanup(&self, now: u64, max_deletions: usize) -> Result<OrphanStreamCleanupStats> {
        cleanup_orphan_session_streams(
            &self.config,
            "local",
            now,
            &OrphanStreamCleanupOptions {
                grace_secs: GRACE_SECS,
                max_deletions,
            },
        )
        .await
    }

    async fn guarded_delete(&self, key: &str) -> Result<GuardedOrphanDelete> {
        delete_orphan_stream_guarded(
            &self.config,
            "local",
            key,
            OrphanStreamCleanupWindow {
                now_unix_secs: wall_clock_secs() + GRACE_SECS + 60,
                grace_secs: GRACE_SECS,
            },
        )
        .await
    }

    async fn assert_active_retained(&self, stats: &OrphanStreamCleanupStats, key: &str) {
        assert_eq!((stats.deleted, stats.skipped_active), (0, 1));
        assert!(
            stream_exists(&self.jetstream, key).await,
            "active stream missing: {key}"
        );
    }

    async fn remaining_streams(&self, keys: &[String]) -> usize {
        let mut remaining = 0;
        for key in keys {
            if stream_exists(&self.jetstream, key).await {
                remaining += 1;
            }
        }
        remaining
    }
}

fn assert_inventory_keys(inventory: &OrphanStreamInventory, expected: &[&str]) {
    let mut actual: Vec<_> = inventory
        .orphans
        .iter()
        .map(|o| o.storage_key.as_str())
        .collect();
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(
        actual, expected,
        "only metadata-less sessions should be reported"
    );
}

fn orphan_report<'a>(inventory: &'a OrphanStreamInventory, key: &str) -> &'a OrphanStreamReport {
    inventory
        .orphans
        .iter()
        .find(|o| o.storage_key == key)
        .expect("orphan report")
}

fn assert_empty_orphan(orphan: &OrphanStreamReport, key: &str, name: &str) {
    assert_eq!(
        orphan,
        &OrphanStreamReport {
            stream_name: name.into(),
            storage_key: key.into(),
            created_unix_secs: orphan.created_unix_secs,
            messages: 0,
            bytes: 0,
            last_message_unix_secs: None,
            session_lease_present: false,
            a2a_lease_present: false,
            a2a_unresolved_tasks: false,
        }
    );
}

fn assert_recent(created: u64) {
    let now = wall_clock_secs();
    assert!(created <= now);
    assert!(created + GRACE_SECS > now);
}

fn assert_busy_orphan(busy: &OrphanStreamReport) {
    assert_eq!(busy.messages, 2);
    assert!(busy.bytes > 0);
    assert!(busy.last_message_unix_secs.is_some());
}

fn assert_unmapped_inventory(inventory: &OrphanStreamInventory) {
    assert_eq!(inventory.unmapped.len(), 1);
    assert_eq!(inventory.unmapped[0].stream_name, "SESSION_unmappable");
    assert!(!inventory.unmapped[0].reason.is_empty());
}

fn assert_batch_counts(stats: &OrphanStreamCleanupStats) {
    assert_eq!(
        (stats.deleted, stats.scanned, stats.deferred, stats.unmapped),
        (2, 3, 1, 1),
        "batch cap must defer candidates without hiding them"
    );
}

async fn assert_empty_cleanup(
    session_id: &str,
    clock_offset: u64,
    expected: OrphanStreamCleanupStats,
) -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key(session_id);
    seed_orphan_stream(&fixture.jetstream, &key, 0).await?;
    let stats = fixture.cleanup(wall_clock_secs() + clock_offset, 5).await?;
    assert_eq!(stats, expected);
    assert_eq!(
        stream_exists(&fixture.jetstream, &key).await,
        expected.deleted == 0
    );
    Ok(())
}

async fn assert_periodic_cleanup(
    session_id: &str,
    clock_offset: u64,
    expected: RemoteCleanupStats,
) -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key(session_id);
    seed_orphan_stream(&fixture.jetstream, &key, 0).await?;
    let now = wall_clock_secs() + clock_offset;
    let gc_session_id = format!("{session_id}-streams");
    let outcome = run_periodic_remote_cleanup_with(
        &fixture.config,
        1,
        "local",
        PeriodicCleanupOverrides {
            gc_session_id: &gc_session_id,
            epoch_hour: now / 3_600,
            now_unix_secs: now,
        },
    )
    .await;
    let should_exist = expected.orphan_deleted == 0;
    assert_eq!(outcome, CleanupOutcome::Ran(expected));
    assert_eq!(stream_exists(&fixture.jetstream, &key).await, should_exist);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_stream_inventory_reports_streams_without_metadata() -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };

    // A live session (stream + metadata) must never be reported.
    let live_id = "inventory-live";
    seed_session_metadata(&fixture.jetstream, live_id).await?;
    seed_orphan_stream(&fixture.jetstream, &storage_key(live_id), 1).await?;

    // An empty orphan, the state GH #2650 leaves behind.
    let orphan_id = "inventory-orphan";
    let orphan_key = storage_key(orphan_id);
    let orphan_name = seed_orphan_stream(&fixture.jetstream, &orphan_key, 0).await?;

    // A non-empty orphan still counts as an orphan, with message data.
    let busy_id = "inventory-orphan-busy";
    let busy_key = storage_key(busy_id);
    seed_orphan_stream(&fixture.jetstream, &busy_key, 2).await?;

    // A SESSION_ stream whose subject cannot map back to a storage key.
    fixture
        .jetstream
        .create_stream(stream::Config {
            name: "SESSION_unmappable".into(),
            subjects: vec!["not.a.session.subject".into()],
            ..Default::default()
        })
        .await?;

    let inventory = list_orphan_session_streams(&fixture.config, "local").await?;

    assert_inventory_keys(&inventory, &[&orphan_key, &busy_key]);
    let orphan = orphan_report(&inventory, &orphan_key);
    assert_empty_orphan(orphan, &orphan_key, &orphan_name);
    assert_recent(orphan.created_unix_secs);
    assert_busy_orphan(orphan_report(&inventory, &busy_key));
    assert_unmapped_inventory(&inventory);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_stream_cleanup_deletes_old_empty_orphan() -> Result<()> {
    // Inject a clock beyond the grace window.
    assert_empty_cleanup(
        "cleanup-old",
        GRACE_SECS + 60,
        OrphanStreamCleanupStats {
            scanned: 1,
            deleted: 1,
            ..Default::default()
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_empty_orphan_stream_is_quarantined_not_deleted() -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-quarantine");
    seed_orphan_stream(&fixture.jetstream, &key, 3).await?;
    let now = wall_clock_secs() + GRACE_SECS + 60;

    let stats = fixture.cleanup(now, 5).await?;

    assert_eq!(stats.deleted, 0);
    assert_eq!(stats.quarantined, 1);
    assert!(stream_exists(&fixture.jetstream, &key).await);
    // The single-stream guarded delete refuses it too.
    let guarded = fixture.guarded_delete(&key).await?;
    assert_eq!(guarded, GuardedOrphanDelete::NonEmpty { messages: 3 });
    assert!(stream_exists(&fixture.jetstream, &key).await);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_stream_within_grace_period_is_skipped() -> Result<()> {
    assert_empty_cleanup(
        "cleanup-recent",
        0,
        OrphanStreamCleanupStats {
            scanned: 1,
            skipped_recent: 1,
            ..Default::default()
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_written_after_inventory_prevents_deletion() -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };
    let session_id = "cleanup-metadata-late";
    let key = storage_key(session_id);
    seed_orphan_stream(&fixture.jetstream, &key, 0).await?;

    // Inventory sees the orphan...
    let inventory = list_orphan_session_streams(&fixture.config, "local").await?;
    assert_eq!(inventory.orphans.len(), 1);
    assert_eq!(inventory.orphans[0].storage_key, key);

    // ...then the session's metadata write lands, racing the cleanup.
    seed_session_metadata(&fixture.jetstream, session_id).await?;

    // The guarded delete — the exact path the cleanup calls per candidate —
    // re-reads metadata from the leader right before deleting.
    let guarded = fixture.guarded_delete(&key).await?;
    assert_eq!(guarded, GuardedOrphanDelete::MetadataPresent);

    // And a full pass leaves the stream alone.
    let now = wall_clock_secs() + GRACE_SECS + 60;
    let stats = fixture.cleanup(now, 5).await?;
    assert_eq!(stats.deleted, 0);
    assert!(stream_exists(&fixture.jetstream, &key).await);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_lease_prevents_orphan_stream_deletion() -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-leased");
    seed_orphan_stream(&fixture.jetstream, &key, 0).await?;
    let _lease =
        worker::acquire_worker_lease(&fixture.jetstream, &key, "orphan-lease-holder").await?;
    let now = wall_clock_secs() + GRACE_SECS + 60;

    let stats = fixture.cleanup(now, 5).await?;

    fixture.assert_active_retained(&stats, &key).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2a_lease_prevents_orphan_stream_deletion() -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-a2a-leased");
    seed_orphan_stream(&fixture.jetstream, &key, 0).await?;
    let lease_bucket = fixture
        .jetstream
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: "harnx_leases".into(),
            history: 5,
            ..Default::default()
        })
        .await?;
    lease_bucket
        .put(format!("sessions/{key}/a2a/lock"), "a2a-lease".into())
        .await?;
    let now = wall_clock_secs() + GRACE_SECS + 60;

    let stats = fixture.cleanup(now, 5).await?;

    fixture.assert_active_retained(&stats, &key).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unresolved_a2a_task_prevents_orphan_stream_deletion() -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-a2a-unresolved");
    seed_orphan_stream(&fixture.jetstream, &key, 0).await?;
    let metadata = fixture.jetstream.get_key_value("harnx_sessions").await?;
    metadata
        .put(
            format!("sessions/{key}/a2a/context"),
            r#"{"state":{"active":{"snapshot":{"task":{"status":{"state":"TASK_STATE_WORKING"}}}}}}"#.into(),
        )
        .await?;
    let now = wall_clock_secs() + GRACE_SECS + 60;

    let stats = fixture.cleanup(now, 5).await?;

    fixture.assert_active_retained(&stats, &key).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_stream_cleanup_honors_batch_limit_and_keeps_unmapped() -> Result<()> {
    let Some(fixture) = OrphanFixture::start().await? else {
        return Ok(());
    };
    let keys: Vec<String> = (0..3)
        .map(|i| storage_key(&format!("cleanup-batch-{i}")))
        .collect();
    for key in &keys {
        seed_orphan_stream(&fixture.jetstream, key, 0).await?;
    }
    fixture
        .jetstream
        .create_stream(stream::Config {
            name: "SESSION_unmapped-batch".into(),
            subjects: vec!["other.subject".into()],
            ..Default::default()
        })
        .await?;
    let now = wall_clock_secs() + GRACE_SECS + 60;

    let stats = fixture.cleanup(now, 2).await?;

    assert_batch_counts(&stats);
    assert_eq!(
        fixture.remaining_streams(&keys).await,
        1,
        "exactly one orphan should remain for the next run"
    );
    assert!(
        fixture
            .jetstream
            .get_stream("SESSION_unmapped-batch")
            .await
            .is_ok(),
        "unmapped stream must never be deleted"
    );

    // The next pass collects the remainder.
    let stats = fixture.cleanup(now, 2).await?;
    assert_eq!(stats.deleted, 1);
    for key in &keys {
        assert!(!stream_exists(&fixture.jetstream, key).await);
    }
    Ok(())
}

/// The periodic GC pass picks up orphan streams too: same leader lease, same
/// hourly dedup, retention window as the grace period (GH #2650).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn periodic_gc_collects_orphan_streams() -> Result<()> {
    // days = 1, so two days puts the stream beyond retention.
    assert_periodic_cleanup(
        "periodic-gc-orphan",
        2 * GRACE_SECS,
        RemoteCleanupStats {
            orphan_streams: 1,
            orphan_deleted: 1,
            ..Default::default()
        },
    )
    .await
}

/// A fresh orphan created by an in-flight session must survive a GC pass that
/// runs with the real clock: the retention window is the grace period.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn periodic_gc_keeps_fresh_orphan_streams() -> Result<()> {
    assert_periodic_cleanup(
        "periodic-gc-fresh-orphan",
        0,
        RemoteCleanupStats {
            orphan_streams: 1,
            orphan_skipped: 1,
            ..Default::default()
        },
    )
    .await
}
