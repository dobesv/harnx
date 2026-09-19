use crate::config::Config;
use crate::nats_admin::{self, kv_bucket_missing};
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_session_metadata::{ListedSession, SessionMetadataStore, SESSION_METADATA_BUCKET};
use async_nats::jetstream::kv::{Operation, Store};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SECONDS_PER_HOUR: u64 = 60 * 60;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const GC_SESSION_ID: &str = "session_metadata_gc";
/// KV key in `harnx_leases` recording the epoch-hour of the last completed periodic GC pass;
/// used to dedup staggered workers to one pass per wall-clock hour.
pub const GC_LAST_RUN_EPOCH_HOUR_KEY: &str = "session_metadata_gc/last_run_epoch_hour";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RemoteCleanupStats {
    pub scanned: usize,
    pub deleted: usize,
    pub skipped_active: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    NotLeader,
    Skipped,
    Failed,
    Ran(RemoteCleanupStats),
}

pub async fn run_remote_cleanup(config: &Config, days: u64, cluster: &str) -> RemoteCleanupStats {
    if days == 0 {
        return RemoteCleanupStats::default();
    }
    run_remote_cleanup_with_gc_id(config, days, cluster, GC_SESSION_ID).await
}

pub async fn run_periodic_remote_cleanup(
    config: &Config,
    days: u64,
    cluster: &str,
) -> CleanupOutcome {
    // Zero explicitly disables cleanup, so don't acquire a lease or create marker state.
    if days == 0 {
        return CleanupOutcome::Skipped;
    }
    run_periodic_remote_cleanup_with(
        config,
        days,
        cluster,
        GC_SESSION_ID,
        epoch_hour(now_unix_secs()),
    )
    .await
}

async fn run_remote_cleanup_with_gc_id(
    config: &Config,
    days: u64,
    cluster: &str,
    gc_session_id: &str,
) -> RemoteCleanupStats {
    let lease = match acquire_gc_lease(config, cluster, gc_session_id).await {
        Ok(Some(lease)) => lease,
        Ok(None) => return RemoteCleanupStats::default(),
        Err(error) => {
            warn!(
                "remote session cleanup leader-election failed: cluster={} err={error:#}",
                cluster
            );
            return RemoteCleanupStats {
                errors: 1,
                ..RemoteCleanupStats::default()
            };
        }
    };

    let stats = run_cleanup_pass(config, days, cluster).await;
    release_gc_lease(lease, cluster).await;
    stats
}

#[doc(hidden)]
pub async fn run_periodic_remote_cleanup_with(
    config: &Config,
    days: u64,
    cluster: &str,
    gc_session_id: &str,
    epoch_hour: u64,
) -> CleanupOutcome {
    // Match the public entry point when callers inject a slot for deterministic tests.
    if days == 0 {
        return CleanupOutcome::Skipped;
    }

    let lease = match acquire_gc_lease(config, cluster, gc_session_id).await {
        Ok(Some(lease)) => lease,
        Ok(None) => return CleanupOutcome::NotLeader,
        Err(error) => {
            warn!(
                "periodic remote session cleanup leader-election failed: cluster={} err={error:#}",
                cluster
            );
            return CleanupOutcome::Failed;
        }
    };

    let last_run_epoch_hour = match load_last_run_epoch_hour(config, cluster).await {
        Ok(last_run_epoch_hour) => last_run_epoch_hour,
        Err(error) => {
            warn!(
                "periodic remote session cleanup marker read failed: cluster={} err={error:#}",
                cluster
            );
            release_gc_lease(lease, cluster).await;
            return CleanupOutcome::Failed;
        }
    };

    if last_run_epoch_hour == Some(epoch_hour) {
        release_gc_lease(lease, cluster).await;
        return CleanupOutcome::Skipped;
    }

    let stats = match run_remote_cleanup_inner(config, days, cluster).await {
        Ok(stats) => stats,
        Err(error) => {
            warn!(
                "remote session cleanup failed: cluster={} days={} err={error:#}",
                cluster, days
            );
            release_gc_lease(lease, cluster).await;
            return CleanupOutcome::Failed;
        }
    };
    let marker_write = store_last_run_epoch_hour(config, cluster, epoch_hour).await;
    release_gc_lease(lease, cluster).await;
    outcome_after_marker_write(stats, cluster, marker_write)
}

/// Fold marker persistence into completed-pass stats. A stale marker can cause
/// a harmless repeat scan, so it doesn't change the fact that this pass ran.
fn outcome_after_marker_write(
    mut stats: RemoteCleanupStats,
    cluster: &str,
    marker_write: anyhow::Result<()>,
) -> CleanupOutcome {
    if let Err(error) = marker_write {
        warn!(
            "periodic remote session cleanup marker write failed: cluster={} err={error:#}",
            cluster
        );
        stats.errors += 1;
    }
    CleanupOutcome::Ran(stats)
}

async fn run_cleanup_pass(config: &Config, days: u64, cluster: &str) -> RemoteCleanupStats {
    match run_remote_cleanup_inner(config, days, cluster).await {
        Ok(stats) => stats,
        Err(error) => {
            warn!(
                "remote session cleanup failed: cluster={} days={} err={error:#}",
                cluster, days
            );
            cleanup_error_stats()
        }
    }
}

fn cleanup_error_stats() -> RemoteCleanupStats {
    RemoteCleanupStats {
        errors: 1,
        ..RemoteCleanupStats::default()
    }
}

async fn release_gc_lease(lease: NatsSessionLease, cluster: &str) {
    if let Err(error) = lease.release().await {
        warn!(
            "remote session cleanup lease release failed: cluster={} err={error:#}",
            cluster
        );
    }
}

async fn load_last_run_epoch_hour(config: &Config, cluster: &str) -> anyhow::Result<Option<u64>> {
    let Some(store) = load_optional_lease_store(config, cluster).await? else {
        return Ok(None);
    };
    let Some(value) = store.get(GC_LAST_RUN_EPOCH_HOUR_KEY).await? else {
        return Ok(None);
    };
    let value = std::str::from_utf8(&value)?;
    Ok(Some(value.parse()?))
}

async fn store_last_run_epoch_hour(
    config: &Config,
    cluster: &str,
    epoch_hour: u64,
) -> anyhow::Result<()> {
    let Some(store) = load_optional_lease_store(config, cluster).await? else {
        return Ok(());
    };
    store
        .put(GC_LAST_RUN_EPOCH_HOUR_KEY, epoch_hour.to_string().into())
        .await?;
    Ok(())
}

struct CandidateContext<'a> {
    config: &'a Config,
    cluster: &'a str,
    metadata_store: &'a SessionMetadataStore,
    lease_store: Option<&'a Store>,
    threshold: u64,
}

async fn run_remote_cleanup_inner(
    config: &Config,
    days: u64,
    cluster: &str,
) -> anyhow::Result<RemoteCleanupStats> {
    let jetstream = config.nats_jetstream(cluster).await?;
    let metadata_kv = match jetstream.get_key_value(SESSION_METADATA_BUCKET).await {
        Ok(store) => store,
        Err(error) if kv_bucket_missing(&error) => return Ok(RemoteCleanupStats::default()),
        Err(error) => return Err(error.into()),
    };
    let metadata_store = SessionMetadataStore::from_store(metadata_kv, jetstream.client().clone());
    let lease_store = load_optional_lease_store(config, cluster).await?;
    let threshold = cleanup_threshold(now_unix_secs(), days);
    let records = metadata_store.list().await?;
    let candidates = candidate_session_ids(&records, threshold);
    let mut stats = RemoteCleanupStats {
        scanned: candidates.len(),
        ..RemoteCleanupStats::default()
    };
    let context = CandidateContext {
        config,
        cluster,
        metadata_store: &metadata_store,
        lease_store: lease_store.as_ref(),
        threshold,
    };

    for session_id in candidates {
        handle_candidate(&context, &session_id, &mut stats).await;
    }

    Ok(stats)
}

async fn acquire_gc_lease(
    config: &Config,
    cluster: &str,
    gc_session_id: &str,
) -> anyhow::Result<Option<NatsSessionLease>> {
    let jetstream = config.nats_jetstream(cluster).await?;
    // `NatsLeaseConfig::default()`'s `replicas` is always 1. Reconcile only
    // ever raises an existing bucket's replicas now (see
    // `reconcile_bucket_replicas`), so pinning that default here can no
    // longer downgrade a bucket another creator already got right — but
    // the worker daemon runs this GC hourly through
    // `run_periodic_remote_cleanup` and could just as easily be the first
    // thing to ever touch `harnx_leases` on a given cluster, in which case
    // the initial `create_key_value` (not reconcile) sets the real replica
    // count. Resolve the cluster's actual configured value so that first
    // creation is already right, instead of leaning on a worker starting
    // later to reconcile it up.
    let replicas = config
        .resolve_nats_server(cluster)
        .await?
        .resolved_replicas();
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream,
        session_id: gc_session_id,
        worker_id: format!("remote-session-cleanup:{}", std::process::id()),
        generation: 0,
        config: NatsLeaseConfig {
            ttl: Duration::from_secs(5),
            renew_interval: Duration::from_secs(1),
            replicas,
            ..NatsLeaseConfig::default()
        },
        session_metadata: None,
    })
    .await
}

async fn load_optional_lease_store(
    config: &Config,
    cluster: &str,
) -> anyhow::Result<Option<Store>> {
    match config.nats_kv_bucket(cluster, "harnx_leases").await {
        Ok(store) => Ok(Some(store)),
        Err(error) => {
            if error
                .chain()
                .find_map(|cause| {
                    cause.downcast_ref::<async_nats::jetstream::context::KeyValueError>()
                })
                .is_some_and(kv_bucket_missing)
            {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

async fn handle_candidate(
    context: &CandidateContext<'_>,
    session_id: &str,
    stats: &mut RemoteCleanupStats,
) {
    match candidate_is_active(context, session_id).await {
        Ok(true) => {
            stats.skipped_active += 1;
        }
        Ok(false) => {
            match nats_admin::delete_remote_session_by_key(
                context.config,
                context.cluster,
                session_id,
            )
            .await
            {
                Ok(_) => stats.deleted += 1,
                Err(error) => {
                    stats.errors += 1;
                    warn!(
                    "remote session cleanup delete failed: cluster={} session_id={} err={error:#}",
                    context.cluster, session_id
                );
                }
            }
        }
        Err(error) => {
            stats.errors += 1;
            warn!(
                "remote session cleanup candidate check failed: cluster={} session_id={} err={error:#}",
                context.cluster, session_id
            );
        }
    }
}

async fn candidate_is_active(
    context: &CandidateContext<'_>,
    session_id: &str,
) -> anyhow::Result<bool> {
    if lease_present(context.lease_store, session_id).await? {
        return Ok(true);
    }

    session_reactivated(context.metadata_store, session_id, context.threshold).await
}

async fn lease_present(lease_store: Option<&Store>, session_id: &str) -> anyhow::Result<bool> {
    let Some(lease_store) = lease_store else {
        return Ok(false);
    };
    let lease_key = NatsLeaseConfig::default().key_for_session(session_id);
    match lease_store.entry(lease_key.clone()).await {
        Ok(Some(entry)) => Ok(matches!(entry.operation, Operation::Put)),
        Ok(None) => Ok(false),
        Err(error) => Err(anyhow::Error::from(error)).map_err(|error| {
            error.context(format!("Failed to inspect session lease key '{lease_key}'"))
        }),
    }
}

async fn session_reactivated(
    metadata_store: &SessionMetadataStore,
    session_id: &str,
    threshold: u64,
) -> anyhow::Result<bool> {
    Ok(
        match metadata_store.get_activity_for_cleanup(session_id).await? {
            Some(activity) => activity.last_activity_at.timestamp() >= threshold as i64,
            None => metadata_store
                .get(session_id)
                .await?
                .is_none_or(|record| record.metadata.created_at.timestamp() >= threshold as i64),
        },
    )
}

fn candidate_session_ids(records: &[ListedSession], threshold: u64) -> Vec<String> {
    records
        .iter()
        .filter(|record| is_cleanup_candidate(record, threshold))
        .map(|record| record.metadata.storage_key())
        .collect()
}

fn is_cleanup_candidate(record: &ListedSession, threshold: u64) -> bool {
    record
        .activity
        .as_ref()
        .map(|activity| activity.last_activity_at)
        .unwrap_or(record.metadata.created_at)
        .timestamp()
        < threshold as i64
}

fn cleanup_threshold(now_unix_secs: u64, days: u64) -> u64 {
    now_unix_secs.saturating_sub(days.saturating_mul(SECONDS_PER_DAY))
}

fn epoch_hour(now_unix_secs: u64) -> u64 {
    now_unix_secs / SECONDS_PER_HOUR
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{
        candidate_session_ids, cleanup_threshold, epoch_hour, lease_present,
        outcome_after_marker_write, run_periodic_remote_cleanup, run_periodic_remote_cleanup_with,
        run_remote_cleanup, run_remote_cleanup_with_gc_id, CleanupOutcome, RemoteCleanupStats,
        SECONDS_PER_DAY, SECONDS_PER_HOUR,
    };
    use crate::config::{Config, NatsServerConfig};
    use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
    use crate::nats_session_log::stream_name_for_session;
    use crate::nats_session_metadata::{
        activity_key, ListedSession, SessionActivity, SessionInitializer, SessionMetadata,
        SessionMetadataStore,
    };
    use async_nats::jetstream::stream;
    use chrono::{TimeZone, Utc};

    fn storage_key(id: &str) -> String {
        harnx_core::session_identity::session_key(Some("hephaestus"), id)
    }

    fn sample_record(session_id: &str, last_activity: u64) -> ListedSession {
        ListedSession {
            metadata: SessionMetadata::new(
                session_id,
                SessionInitializer::named("hephaestus", Default::default()),
            ),
            metadata_revision: 1,
            activity: Some(SessionActivity {
                first_activation_at: None,
                last_activity_at: Utc.timestamp_opt(last_activity as i64, 0).unwrap(),
            }),
            unread: false,
        }
    }

    async fn put_test_metadata(
        jetstream: &async_nats::jetstream::Context,
        record: &ListedSession,
    ) -> SessionMetadataStore {
        let store = SessionMetadataStore::ensure(jetstream, 1)
            .await
            .expect("metadata bucket");
        store
            .create(&record.metadata)
            .await
            .expect("create metadata");
        store
            .kv_store()
            .put(
                activity_key(&record.metadata.storage_key()),
                serde_json::to_vec(record.activity.as_ref().unwrap())
                    .unwrap()
                    .into(),
            )
            .await
            .expect("put activity");
        store
    }

    async fn assert_session_deleted(
        stats: RemoteCleanupStats,
        jetstream: &async_nats::jetstream::Context,
        metadata_store: &SessionMetadataStore,
        session_id: &str,
    ) {
        assert_eq!(stats.errors, 0);
        assert!(!stream_exists(jetstream, session_id).await);
        assert!(metadata_store
            .get(&storage_key(session_id))
            .await
            .expect("get metadata")
            .is_none());
    }

    #[test]
    fn candidate_filter_selects_only_stale_records() {
        let mut missing_stale = sample_record("missing-stale", 200);
        missing_stale.metadata.created_at = Utc.timestamp_opt(99, 0).unwrap();
        missing_stale.activity = None;
        let mut missing_fresh = sample_record("missing-fresh", 1);
        missing_fresh.metadata.created_at = Utc.timestamp_opt(101, 0).unwrap();
        missing_fresh.activity = None;
        let records = vec![
            sample_record("old", 99),
            sample_record("borderline", 100),
            sample_record("fresh", 101),
            missing_stale,
            missing_fresh,
        ];

        assert_eq!(
            candidate_session_ids(&records, 100),
            vec![storage_key("old"), storage_key("missing-stale")]
        );
    }

    #[test]
    fn cleanup_threshold_saturates() {
        assert_eq!(cleanup_threshold(10, 1), 0);
        assert_eq!(
            cleanup_threshold(SECONDS_PER_DAY * 10, 2),
            SECONDS_PER_DAY * 8
        );
    }

    #[test]
    fn epoch_hour_uses_wall_clock_hour_slots() {
        assert_eq!(epoch_hour(0), 0);
        assert_eq!(epoch_hour(SECONDS_PER_HOUR - 1), 0);
        assert_eq!(epoch_hour(SECONDS_PER_HOUR), 1);
        assert_eq!(epoch_hour(SECONDS_PER_HOUR * 17 + 42), 17);
    }

    #[test]
    fn successful_marker_write_preserves_completed_pass_stats() {
        let stats = RemoteCleanupStats {
            scanned: 3,
            deleted: 2,
            skipped_active: 1,
            errors: 0,
        };

        assert_eq!(
            outcome_after_marker_write(stats.clone(), "local", Ok(())),
            CleanupOutcome::Ran(stats)
        );
    }

    #[test]
    fn failed_marker_write_counts_error_but_pass_stays_ran() {
        let stats = RemoteCleanupStats {
            scanned: 3,
            deleted: 2,
            skipped_active: 1,
            errors: 0,
        };
        let expected = RemoteCleanupStats {
            errors: 1,
            ..stats.clone()
        };

        assert_eq!(
            outcome_after_marker_write(stats, "local", Err(anyhow::anyhow!("boom"))),
            CleanupOutcome::Ran(expected)
        );
    }

    #[tokio::test]
    async fn lease_present_returns_false_when_lease_store_missing() {
        assert!(!lease_present(None, "session-123")
            .await
            .expect("missing lease store"));
    }

    #[tokio::test]
    async fn run_remote_cleanup_returns_default_when_days_zero() {
        let config = Config::default();

        assert_eq!(
            run_remote_cleanup(&config, 0, "no-cluster").await,
            RemoteCleanupStats::default()
        );
    }

    #[tokio::test]
    async fn run_periodic_remote_cleanup_skips_when_days_zero() {
        let config = Config::default();

        assert_eq!(
            run_periodic_remote_cleanup(&config, 0, "no-cluster").await,
            CleanupOutcome::Skipped
        );
    }

    #[tokio::test]
    async fn periodic_cleanup_reports_failed_when_lease_acquisition_errors() {
        let config = Config::default();

        assert_eq!(
            run_periodic_remote_cleanup_with(&config, 1, "no-cluster", "gc-test", 42).await,
            CleanupOutcome::Failed
        );
    }

    #[tokio::test]
    async fn missing_metadata_bucket_returns_zero_stats() {
        let Some(server_url) = test_nats_url() else {
            eprintln!(
                "skipping missing_metadata_bucket_returns_zero_stats: HARNX_NATS_TEST_URL unset"
            );
            return;
        };

        let cluster = unique_cluster_name("missing-metadata");
        let config = test_config(&cluster, &server_url);

        let stats = run_remote_cleanup_with_gc_id(
            &config,
            30,
            &cluster,
            &unique_gc_session_id("missing-metadata-gc"),
        )
        .await;

        assert_eq!(stats, RemoteCleanupStats::default());
    }

    #[tokio::test]
    async fn stale_session_without_lease_gets_deleted() {
        let Some(server_url) = test_nats_url() else {
            eprintln!(
                "skipping stale_session_without_lease_gets_deleted: HARNX_NATS_TEST_URL unset"
            );
            return;
        };

        let cluster = unique_cluster_name("stale-delete");
        let config = test_config(&cluster, &server_url);
        let jetstream = config.nats_jetstream(&cluster).await.expect("jetstream");
        let lease_store = config
            .nats_kv_bucket(&cluster, "harnx_leases")
            .await
            .expect("lease bucket");
        let session_id = unique_session_id("stale-delete");
        put_test_stream(&jetstream, &session_id).await;
        let metadata_store = put_test_metadata(&jetstream, &sample_record(&session_id, 1)).await;

        let stats =
            run_remote_cleanup_with_gc_id(&config, 1, &cluster, &unique_gc_session_id("stale-gc"))
                .await;

        assert_session_deleted(stats, &jetstream, &metadata_store, &session_id).await;
        assert!(metadata_store
            .get_activity(&storage_key(&session_id))
            .await
            .expect("get activity")
            .is_none());
        assert!(lease_store
            .entry(NatsLeaseConfig::default().key_for_session(&storage_key(&session_id)))
            .await
            .expect("lease entry")
            .is_none());
    }

    #[tokio::test]
    async fn stale_session_with_missing_activity_gets_deleted() {
        let Some(server_url) = test_nats_url() else {
            eprintln!(
                "skipping stale_session_with_missing_activity_gets_deleted: HARNX_NATS_TEST_URL unset"
            );
            return;
        };

        let cluster = unique_cluster_name("missing-activity-delete");
        let config = test_config(&cluster, &server_url);
        let jetstream = config.nats_jetstream(&cluster).await.expect("jetstream");
        let session_id = unique_session_id("missing-activity-delete");
        put_test_stream(&jetstream, &session_id).await;
        let mut record = sample_record(&session_id, 1);
        record.metadata.created_at = Utc.timestamp_opt(1, 0).unwrap();
        let metadata_store = put_test_metadata(&jetstream, &record).await;
        metadata_store
            .kv_store()
            .purge(activity_key(&storage_key(&session_id)))
            .await
            .expect("purge activity");

        let stats = run_remote_cleanup_with_gc_id(
            &config,
            1,
            &cluster,
            &unique_gc_session_id("missing-activity-gc"),
        )
        .await;

        assert_session_deleted(stats, &jetstream, &metadata_store, &session_id).await;
    }

    #[tokio::test]
    async fn stale_session_with_malformed_activity_gets_deleted() {
        let Some(server_url) = test_nats_url() else {
            eprintln!(
                "skipping stale_session_with_malformed_activity_gets_deleted: HARNX_NATS_TEST_URL unset"
            );
            return;
        };

        let cluster = unique_cluster_name("malformed-activity-delete");
        let config = test_config(&cluster, &server_url);
        let jetstream = config.nats_jetstream(&cluster).await.expect("jetstream");
        let session_id = unique_session_id("malformed-activity-delete");
        put_test_stream(&jetstream, &session_id).await;
        let mut record = sample_record(&session_id, 1);
        record.metadata.created_at = Utc.timestamp_opt(1, 0).unwrap();
        let metadata_store = put_test_metadata(&jetstream, &record).await;
        metadata_store
            .kv_store()
            .put(activity_key(&storage_key(&session_id)), "not-json".into())
            .await
            .expect("corrupt activity");

        let stats = run_remote_cleanup_with_gc_id(
            &config,
            1,
            &cluster,
            &unique_gc_session_id("malformed-activity-gc"),
        )
        .await;

        assert_session_deleted(stats, &jetstream, &metadata_store, &session_id).await;
    }

    #[tokio::test]
    async fn fresh_session_is_not_deleted() {
        let Some(server_url) = test_nats_url() else {
            eprintln!("skipping fresh_session_is_not_deleted: HARNX_NATS_TEST_URL unset");
            return;
        };

        let cluster = unique_cluster_name("fresh-keep");
        let config = test_config(&cluster, &server_url);
        let jetstream = config.nats_jetstream(&cluster).await.expect("jetstream");
        let session_id = unique_session_id("fresh-keep");
        put_test_stream(&jetstream, &session_id).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("now")
            .as_secs();
        let metadata_store = put_test_metadata(&jetstream, &sample_record(&session_id, now)).await;

        let stats =
            run_remote_cleanup_with_gc_id(&config, 1, &cluster, &unique_gc_session_id("fresh-gc"))
                .await;

        assert_eq!(stats.errors, 0);
        assert!(stream_exists(&jetstream, &session_id).await);
        assert!(metadata_store
            .get(&storage_key(&session_id))
            .await
            .expect("get metadata")
            .is_some());
    }

    #[tokio::test]
    async fn leased_session_is_skipped() {
        let Some(server_url) = test_nats_url() else {
            eprintln!("skipping leased_session_is_skipped: HARNX_NATS_TEST_URL unset");
            return;
        };

        let cluster = unique_cluster_name("lease-skip");
        let config = test_config(&cluster, &server_url);
        let jetstream = config.nats_jetstream(&cluster).await.expect("jetstream");
        let session_id = unique_session_id("lease-skip");
        put_test_stream(&jetstream, &session_id).await;
        let metadata_store = put_test_metadata(&jetstream, &sample_record(&session_id, 1)).await;
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: jetstream.clone(),
            session_id: &storage_key(&session_id),
            worker_id: "lease-holder".to_string(),
            generation: 1,
            config: NatsLeaseConfig::default(),
            session_metadata: None,
        })
        .await
        .expect("acquire lease")
        .expect("lease held");

        let stats = run_remote_cleanup_with_gc_id(
            &config,
            1,
            &cluster,
            &unique_gc_session_id("lease-skip-gc"),
        )
        .await;

        assert_eq!(stats.errors, 0);
        assert!(stats.skipped_active >= 1);
        assert!(stream_exists(&jetstream, &session_id).await);
        assert!(metadata_store
            .get(&storage_key(&session_id))
            .await
            .expect("get metadata")
            .is_some());
        assert!(matches!(
            lease_entry_operation(&config, &cluster, &session_id).await,
            Some(async_nats::jetstream::kv::Operation::Put)
        ));
        lease.release().await.expect("release lease");
    }

    #[tokio::test]
    async fn concurrent_cleanup_runs_only_delete_once() {
        let Some(server_url) = test_nats_url() else {
            eprintln!(
                "skipping concurrent_cleanup_runs_only_delete_once: HARNX_NATS_TEST_URL unset"
            );
            return;
        };

        let cluster = unique_cluster_name("leader-race");
        let config = test_config(&cluster, &server_url);
        let jetstream = config.nats_jetstream(&cluster).await.expect("jetstream");
        let session_id = unique_session_id("leader-race");
        put_test_stream(&jetstream, &session_id).await;
        let metadata_store = put_test_metadata(&jetstream, &sample_record(&session_id, 1)).await;

        let gc_session_id = unique_gc_session_id("leader-race-gc");
        let (first, second) = tokio::join!(
            run_remote_cleanup_with_gc_id(&config, 1, &cluster, &gc_session_id),
            run_remote_cleanup_with_gc_id(&config, 1, &cluster, &gc_session_id)
        );

        assert_eq!(first.errors, 0);
        assert_eq!(second.errors, 0);
        assert!(!stream_exists(&jetstream, &session_id).await);
        assert!(metadata_store
            .get(&storage_key(&session_id))
            .await
            .expect("get metadata")
            .is_none());
        assert!(first == Default::default() || second == Default::default());
    }

    fn test_nats_url() -> Option<String> {
        std::env::var("HARNX_NATS_TEST_URL").ok()
    }

    fn unique_cluster_name(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    fn unique_session_id(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    fn unique_gc_session_id(prefix: &str) -> String {
        format!("gc-{prefix}-{}", uuid::Uuid::new_v4())
    }

    fn test_config(cluster: &str, server_url: &str) -> Config {
        let mut config = Config::default();
        config.nats_servers.push(NatsServerConfig {
            name: cluster.to_string(),
            url: server_url.to_string(),
            token: None,
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: Vec::new(),
        });
        config
    }

    async fn put_test_stream(jetstream: &async_nats::jetstream::Context, session_id: &str) {
        let session_id = storage_key(session_id);
        let stream_name = stream_name_for_session(&session_id);
        jetstream
            .create_stream(stream::Config {
                name: stream_name.clone(),
                subjects: vec![format!("sessions.{session_id}.>")],
                storage: stream::StorageType::File,
                ..Default::default()
            })
            .await
            .expect("create stream");
        jetstream
            .publish(format!("sessions.{session_id}.event"), "test".into())
            .await
            .expect("publish ack future")
            .await
            .expect("publish ack");
    }

    async fn lease_entry_operation(
        config: &Config,
        cluster: &str,
        session_id: &str,
    ) -> Option<async_nats::jetstream::kv::Operation> {
        let lease_store = config
            .nats_kv_bucket(cluster, "harnx_leases")
            .await
            .expect("lease bucket");
        let lease_key = NatsLeaseConfig::default().key_for_session(&storage_key(session_id));
        lease_store
            .entry(lease_key)
            .await
            .expect("lease entry")
            .map(|entry| entry.operation)
    }

    async fn stream_exists(jetstream: &async_nats::jetstream::Context, session_id: &str) -> bool {
        jetstream
            .get_stream(&stream_name_for_session(&storage_key(session_id)))
            .await
            .is_ok()
    }
}
