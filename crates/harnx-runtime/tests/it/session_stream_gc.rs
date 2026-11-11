//! Session streams that go unwritten for the retention window are deleted,
//! with or without session metadata, and `harnx list orphan-streams` reports
//! the ones without.
use crate::worker;

use anyhow::Result;
use async_nats::jetstream::stream;
use chrono::{TimeZone, Utc};
use harnx_runtime::nats_session_log::{stream_name_for_session, subject_for_session};
use harnx_runtime::nats_session_metadata::{
    activity_key, SessionActivity, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use harnx_runtime::remote_session_cleanup::{
    run_periodic_remote_cleanup_with, CleanupOutcome, PeriodicCleanupOverrides, RemoteCleanupStats,
};
use harnx_runtime::session_stream_gc::{
    cleanup_session_streams, delete_idle_stream_guarded, GuardedStreamDelete, StreamCleanupOptions,
    StreamCleanupStats, StreamCleanupWindow, StreamScope,
};
use harnx_runtime::session_stream_orphans::{
    list_orphan_session_streams, OrphanStreamInventory, OrphanStreamReport,
};
use worker::{local_nats_runtime_config, require_nats_server};

const AGENT: &str = "orphan-stream-agent";
const IDLE_SECS: u64 = 24 * 60 * 60;

fn storage_key(session_id: &str) -> String {
    harnx_core::session_identity::session_key(Some(AGENT), session_id)
}

fn wall_clock_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_secs()
}

/// A clock just past the idle window, so a stream written now counts as idle.
fn idle_clock() -> u64 {
    wall_clock_secs() + IDLE_SECS + 60
}

/// Ensure the session metadata bucket exists. Stream cleanup deliberately
/// refuses to run without it: metadata presence is read for every candidate,
/// and the metadata GC also skips clusters without the bucket.
async fn ensure_metadata_bucket(jetstream: &async_nats::jetstream::Context) -> Result<()> {
    SessionMetadataStore::ensure(jetstream, 1).await.map(|_| ())
}

/// A `SESSION_` stream for `storage_key` holding `messages` messages.
async fn seed_stream(
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
            .publish(subject.clone(), "entry".into())
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

/// Record activity for `storage_key` at `at_unix_secs`, as a reader touching
/// the session would.
async fn seed_activity(
    store: &SessionMetadataStore,
    storage_key: &str,
    at_unix_secs: u64,
) -> Result<()> {
    let activity = SessionActivity {
        first_activation_at: None,
        last_activity_at: Utc.timestamp_opt(at_unix_secs as i64, 0).unwrap(),
    };
    store
        .kv_store()
        .put(
            activity_key(storage_key),
            serde_json::to_vec(&activity)?.into(),
        )
        .await?;
    Ok(())
}

async fn stream_exists(jetstream: &async_nats::jetstream::Context, storage_key: &str) -> bool {
    jetstream
        .get_stream(&stream_name_for_session(storage_key))
        .await
        .is_ok()
}

struct StreamFixture {
    config: harnx_runtime::config::Config,
    jetstream: async_nats::jetstream::Context,
    _server: crate::common::NatsServerHandle,
}

impl StreamFixture {
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

    async fn cleanup(
        &self,
        now: u64,
        max_deletions: usize,
        scope: StreamScope,
    ) -> Result<StreamCleanupStats> {
        cleanup_session_streams(
            &self.config,
            "local",
            now,
            &StreamCleanupOptions {
                idle_secs: IDLE_SECS,
                max_deletions,
                scope,
            },
        )
        .await
    }

    async fn guarded_delete(&self, key: &str, scope: StreamScope) -> Result<GuardedStreamDelete> {
        delete_idle_stream_guarded(
            &self.config,
            "local",
            key,
            StreamCleanupWindow {
                now_unix_secs: idle_clock(),
                idle_secs: IDLE_SECS,
                scope,
            },
        )
        .await
    }

    async fn assert_active_retained(&self, stats: &StreamCleanupStats, key: &str) {
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

    async fn periodic_cleanup(&self, gc_session_id: &str, now: u64) -> CleanupOutcome {
        run_periodic_remote_cleanup_with(
            &self.config,
            1,
            "local",
            PeriodicCleanupOverrides {
                gc_session_id,
                epoch_hour: now / 3_600,
                now_unix_secs: now,
            },
        )
        .await
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
    assert!(created + IDLE_SECS > now);
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

fn assert_batch_counts(stats: &StreamCleanupStats) {
    assert_eq!(
        (stats.idle, stats.deleted, stats.deferred, stats.unmapped),
        (3, 2, 1, 1),
        "batch cap must defer candidates without hiding them"
    );
}

/// An empty orphan stream, cleaned up at `now`.
async fn assert_empty_orphan_cleanup(
    session_id: &str,
    now: u64,
    expected: StreamCleanupStats,
) -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key(session_id);
    seed_stream(&fixture.jetstream, &key, 0).await?;
    let stats = fixture.cleanup(now, 5, StreamScope::Orphans).await?;
    assert_eq!(stats, expected);
    assert_eq!(
        stream_exists(&fixture.jetstream, &key).await,
        expected.deleted == 0
    );
    Ok(())
}

/// An empty orphan stream, through the worker's hourly GC at `now`.
async fn assert_periodic_orphan_cleanup(
    session_id: &str,
    now: u64,
    expected: RemoteCleanupStats,
) -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key(session_id);
    seed_stream(&fixture.jetstream, &key, 0).await?;
    let outcome = fixture
        .periodic_cleanup(&format!("{session_id}-streams"), now)
        .await;
    let should_exist = expected.idle_streams_deleted == 0;
    assert_eq!(outcome, CleanupOutcome::Ran(expected));
    assert_eq!(stream_exists(&fixture.jetstream, &key).await, should_exist);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_stream_inventory_reports_streams_without_metadata() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };

    // A live session (stream + metadata) must never be reported.
    let live_id = "inventory-live";
    seed_session_metadata(&fixture.jetstream, live_id).await?;
    seed_stream(&fixture.jetstream, &storage_key(live_id), 1).await?;

    // An empty orphan, the state a failed metadata write leaves behind.
    let orphan_id = "inventory-orphan";
    let orphan_key = storage_key(orphan_id);
    let orphan_name = seed_stream(&fixture.jetstream, &orphan_key, 0).await?;

    // A non-empty orphan still counts as an orphan, with message data.
    let busy_id = "inventory-orphan-busy";
    let busy_key = storage_key(busy_id);
    seed_stream(&fixture.jetstream, &busy_key, 2).await?;

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
async fn an_idle_empty_orphan_is_deleted() -> Result<()> {
    assert_empty_orphan_cleanup(
        "cleanup-old",
        idle_clock(),
        StreamCleanupStats {
            streams: 1,
            idle: 1,
            deleted: 1,
            ..Default::default()
        },
    )
    .await
}

/// An orphan that holds messages goes too, once its last write is past the
/// window, and the deletion takes the session's other leftovers with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_orphan_with_messages_is_deleted_with_its_leftovers() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-messages");
    seed_stream(&fixture.jetstream, &key, 3).await?;
    let metadata = fixture.jetstream.get_key_value("harnx_sessions").await?;
    metadata
        .put(activity_key(&key), "left behind".into())
        .await?;

    let stats = fixture
        .cleanup(idle_clock(), 5, StreamScope::Orphans)
        .await?;

    assert_eq!((stats.idle, stats.deleted, stats.errors), (1, 1, 0));
    assert!(!stream_exists(&fixture.jetstream, &key).await);
    assert_eq!(metadata.get(activity_key(&key)).await?, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_orphan_written_within_the_window_is_kept() -> Result<()> {
    assert_empty_orphan_cleanup(
        "cleanup-recent",
        wall_clock_secs(),
        StreamCleanupStats {
            streams: 1,
            ..Default::default()
        },
    )
    .await
}

/// The guarded delete reads the stream's age again just before deleting, so
/// a stream written after the listing picked it is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_guarded_delete_keeps_a_stream_written_within_the_window() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-rewritten");
    seed_stream(&fixture.jetstream, &key, 1).await?;

    let guarded = delete_idle_stream_guarded(
        &fixture.config,
        "local",
        &key,
        StreamCleanupWindow {
            now_unix_secs: wall_clock_secs(),
            idle_secs: IDLE_SECS,
            scope: StreamScope::All,
        },
    )
    .await?;

    assert_eq!(guarded, GuardedStreamDelete::Recent);
    assert!(stream_exists(&fixture.jetstream, &key).await);
    Ok(())
}

/// A session whose readers keep its activity record fresh, but whose stream
/// nothing has written for the window, is deleted by the hourly GC.
/// `harnx delete orphan-streams` leaves it alone, since it has metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_session_with_recent_activity_is_deleted_by_the_hourly_gc() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let session_id = "cleanup-read-only";
    let key = storage_key(session_id);
    let store = seed_session_metadata(&fixture.jetstream, session_id).await?;
    seed_stream(&fixture.jetstream, &key, 2).await?;
    let now = idle_clock();
    seed_activity(&store, &key, now).await?;

    let orphans_only = fixture.cleanup(now, 5, StreamScope::Orphans).await?;
    assert_eq!(
        (orphans_only.deleted, orphans_only.skipped_metadata),
        (0, 1)
    );
    assert!(stream_exists(&fixture.jetstream, &key).await);

    let outcome = fixture
        .periodic_cleanup(&format!("{session_id}-gc"), now)
        .await;

    assert_eq!(
        outcome,
        CleanupOutcome::Ran(RemoteCleanupStats {
            idle_streams: 1,
            idle_streams_deleted: 1,
            ..Default::default()
        }),
        "the activity scan keeps it, the stream scan deletes it"
    );
    assert!(!stream_exists(&fixture.jetstream, &key).await);
    assert!(store.get(&key).await?.is_none(), "metadata goes too");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_written_after_listing_keeps_an_orphan_in_the_orphans_scope() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let session_id = "cleanup-metadata-late";
    let key = storage_key(session_id);
    seed_stream(&fixture.jetstream, &key, 0).await?;

    // The listing sees the orphan...
    let inventory = list_orphan_session_streams(&fixture.config, "local").await?;
    assert_eq!(inventory.orphans.len(), 1);
    assert_eq!(inventory.orphans[0].storage_key, key);

    // ...then the session's metadata write lands, racing the cleanup.
    seed_session_metadata(&fixture.jetstream, session_id).await?;

    // The guarded delete, the exact path the cleanup takes per candidate,
    // reads metadata from the leader right before deleting.
    let guarded = fixture.guarded_delete(&key, StreamScope::Orphans).await?;
    assert_eq!(guarded, GuardedStreamDelete::MetadataPresent);

    // And a full pass leaves the stream alone.
    let stats = fixture
        .cleanup(idle_clock(), 5, StreamScope::Orphans)
        .await?;
    assert_eq!((stats.deleted, stats.skipped_metadata), (0, 1));
    assert!(stream_exists(&fixture.jetstream, &key).await);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_lease_prevents_idle_stream_deletion() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-leased");
    seed_stream(&fixture.jetstream, &key, 0).await?;
    let _lease =
        worker::acquire_worker_lease(&fixture.jetstream, &key, "orphan-lease-holder").await?;

    let stats = fixture.cleanup(idle_clock(), 5, StreamScope::All).await?;

    fixture.assert_active_retained(&stats, &key).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2a_lease_prevents_idle_stream_deletion() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-a2a-leased");
    seed_stream(&fixture.jetstream, &key, 0).await?;
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

    let stats = fixture.cleanup(idle_clock(), 5, StreamScope::All).await?;

    fixture.assert_active_retained(&stats, &key).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unresolved_a2a_task_prevents_idle_stream_deletion() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let key = storage_key("cleanup-a2a-unresolved");
    seed_stream(&fixture.jetstream, &key, 0).await?;
    let metadata = fixture.jetstream.get_key_value("harnx_sessions").await?;
    metadata
        .put(
            format!("sessions/{key}/a2a/context"),
            r#"{"state":{"active":{"snapshot":{"task":{"status":{"state":"TASK_STATE_WORKING"}}}}}}"#.into(),
        )
        .await?;

    let stats = fixture.cleanup(idle_clock(), 5, StreamScope::All).await?;

    fixture.assert_active_retained(&stats, &key).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_stream_cleanup_honors_batch_limit_and_keeps_unmapped() -> Result<()> {
    let Some(fixture) = StreamFixture::start().await? else {
        return Ok(());
    };
    let keys: Vec<String> = (0..3)
        .map(|i| storage_key(&format!("cleanup-batch-{i}")))
        .collect();
    for key in &keys {
        seed_stream(&fixture.jetstream, key, 1).await?;
    }
    fixture
        .jetstream
        .create_stream(stream::Config {
            name: "SESSION_unmapped-batch".into(),
            subjects: vec!["other.subject".into()],
            ..Default::default()
        })
        .await?;
    let now = idle_clock();

    let stats = fixture.cleanup(now, 2, StreamScope::All).await?;

    assert_batch_counts(&stats);
    assert_eq!(
        fixture.remaining_streams(&keys).await,
        1,
        "exactly one idle stream should remain for the next run"
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
    let stats = fixture.cleanup(now, 2, StreamScope::All).await?;
    assert_eq!(stats.deleted, 1);
    for key in &keys {
        assert!(!stream_exists(&fixture.jetstream, key).await);
    }
    Ok(())
}

/// The hourly GC picks up orphan streams: same leader lease, same hourly
/// dedup, retention window as the idle window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn periodic_gc_collects_idle_orphan_streams() -> Result<()> {
    // days = 1, so two days puts the stream past the window.
    assert_periodic_orphan_cleanup(
        "periodic-gc-orphan",
        wall_clock_secs() + 2 * IDLE_SECS,
        RemoteCleanupStats {
            idle_streams: 1,
            idle_streams_deleted: 1,
            ..Default::default()
        },
    )
    .await
}

/// A fresh orphan created by an in-flight session must survive a GC pass
/// that runs with the real clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn periodic_gc_keeps_fresh_orphan_streams() -> Result<()> {
    assert_periodic_orphan_cleanup(
        "periodic-gc-fresh-orphan",
        wall_clock_secs(),
        RemoteCleanupStats::default(),
    )
    .await
}
