use crate::config::Config;
use crate::nats_admin::{self, kv_bucket_missing};
use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
use crate::nats_session_index::{self, SessionIndexRecord, SESSION_INDEX_BUCKET};
use async_nats::jetstream::kv::{Operation, Store};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
const GC_SESSION_ID: &str = "session_index_gc";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RemoteCleanupStats {
    pub scanned: usize,
    pub deleted: usize,
    pub skipped_active: usize,
    pub errors: usize,
}

pub async fn run_remote_cleanup(config: &Config, days: u64, cluster: &str) -> RemoteCleanupStats {
    if days == 0 {
        return RemoteCleanupStats::default();
    }
    run_remote_cleanup_with_gc_id(config, days, cluster, GC_SESSION_ID).await
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

    let stats = match run_remote_cleanup_inner(config, days, cluster).await {
        Ok(stats) => stats,
        Err(error) => {
            warn!(
                "remote session cleanup failed: cluster={} days={} err={error:#}",
                cluster, days
            );
            RemoteCleanupStats {
                errors: 1,
                ..RemoteCleanupStats::default()
            }
        }
    };

    if let Err(error) = lease.release().await {
        warn!(
            "remote session cleanup lease release failed: cluster={} err={error:#}",
            cluster
        );
    }

    stats
}

struct CandidateContext<'a> {
    config: &'a Config,
    cluster: &'a str,
    index_store: &'a Store,
    lease_store: Option<&'a Store>,
    threshold: u64,
}

async fn run_remote_cleanup_inner(
    config: &Config,
    days: u64,
    cluster: &str,
) -> anyhow::Result<RemoteCleanupStats> {
    let jetstream = config.nats_jetstream(cluster).await?;
    let index_store = match jetstream.get_key_value(SESSION_INDEX_BUCKET).await {
        Ok(store) => store,
        Err(error) if kv_bucket_missing(&error) => return Ok(RemoteCleanupStats::default()),
        Err(error) => return Err(error.into()),
    };
    let lease_store = load_optional_lease_store(config, cluster).await?;
    let threshold = cleanup_threshold(now_unix_secs(), days);
    let records = nats_session_index::list_records(&index_store).await?;
    let candidates = candidate_session_ids(&records, threshold);
    let mut stats = RemoteCleanupStats {
        scanned: candidates.len(),
        ..RemoteCleanupStats::default()
    };
    let context = CandidateContext {
        config,
        cluster,
        index_store: &index_store,
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
    NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream,
        session_id: gc_session_id,
        worker_id: format!("remote-session-cleanup:{}", std::process::id()),
        generation: 0,
        config: NatsLeaseConfig {
            ttl: Duration::from_secs(5),
            renew_interval: Duration::from_secs(1),
            ..NatsLeaseConfig::default()
        },
        session_index: None,
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
            match nats_admin::delete_remote_session(context.config, context.cluster, session_id)
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

    session_reactivated(context.index_store, session_id, context.threshold).await
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
    index_store: &Store,
    session_id: &str,
    threshold: u64,
) -> anyhow::Result<bool> {
    Ok(
        match nats_session_index::get_record(index_store, session_id).await? {
            Some(record) => record.last_activity >= threshold,
            None => true,
        },
    )
}

fn candidate_session_ids(records: &[SessionIndexRecord], threshold: u64) -> Vec<String> {
    records
        .iter()
        .filter(|record| is_cleanup_candidate(record, threshold))
        .map(|record| record.session_id.clone())
        .collect()
}

fn is_cleanup_candidate(record: &SessionIndexRecord, threshold: u64) -> bool {
    record.last_activity < threshold
}

fn cleanup_threshold(now_unix_secs: u64, days: u64) -> u64 {
    now_unix_secs.saturating_sub(days.saturating_mul(SECONDS_PER_DAY))
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
        candidate_session_ids, cleanup_threshold, lease_present, run_remote_cleanup,
        run_remote_cleanup_with_gc_id, RemoteCleanupStats, SECONDS_PER_DAY,
    };
    use crate::config::{Config, NatsServerConfig};
    use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig, NatsSessionLease};
    use crate::nats_session_index::{self, SessionIndexRecord};
    use crate::nats_session_log::stream_name_for_session;
    use async_nats::jetstream::stream;

    fn sample_record(session_id: &str, last_activity: u64) -> SessionIndexRecord {
        SessionIndexRecord {
            session_id: session_id.to_string(),
            agent_name: "hephaestus".to_string(),
            working_dir: Some("/tmp/project".to_string()),
            git_branch: Some("main".to_string()),
            git_remote: Some("git@github.com:dobesv/harnx.git".to_string()),
            last_activity,
        }
    }

    #[test]
    fn candidate_filter_selects_only_stale_records() {
        let records = vec![
            sample_record("old", 99),
            sample_record("borderline", 100),
            sample_record("fresh", 101),
        ];

        assert_eq!(
            candidate_session_ids(&records, 100),
            vec!["old".to_string()]
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
    async fn missing_index_bucket_returns_zero_stats() {
        let Some(server_url) = test_nats_url() else {
            eprintln!(
                "skipping missing_index_bucket_returns_zero_stats: HARNX_NATS_TEST_URL unset"
            );
            return;
        };

        let cluster = unique_cluster_name("missing-index");
        let config = test_config(&cluster, &server_url);

        let stats = run_remote_cleanup_with_gc_id(
            &config,
            30,
            &cluster,
            &unique_gc_session_id("missing-index-gc"),
        )
        .await;

        assert_eq!(stats.errors, 0);
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
        let index_store = nats_session_index::ensure_index_bucket(&jetstream)
            .await
            .expect("index bucket");
        let lease_store = config
            .nats_kv_bucket(&cluster, "harnx_leases")
            .await
            .expect("lease bucket");
        let session_id = unique_session_id("stale-delete");
        put_test_stream(&jetstream, &session_id).await;
        nats_session_index::put_record(&index_store, &sample_record(&session_id, 1))
            .await
            .expect("put record");

        let stats =
            run_remote_cleanup_with_gc_id(&config, 1, &cluster, &unique_gc_session_id("stale-gc"))
                .await;

        assert_eq!(stats.errors, 0);
        assert!(!stream_exists(&jetstream, &session_id).await);
        assert!(nats_session_index::get_record(&index_store, &session_id)
            .await
            .expect("get record")
            .is_none());
        assert!(lease_store
            .entry(NatsLeaseConfig::default().key_for_session(&session_id))
            .await
            .expect("lease entry")
            .is_none());
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
        let index_store = nats_session_index::ensure_index_bucket(&jetstream)
            .await
            .expect("index bucket");
        let session_id = unique_session_id("fresh-keep");
        put_test_stream(&jetstream, &session_id).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("now")
            .as_secs();
        nats_session_index::put_record(&index_store, &sample_record(&session_id, now))
            .await
            .expect("put record");

        let stats =
            run_remote_cleanup_with_gc_id(&config, 1, &cluster, &unique_gc_session_id("fresh-gc"))
                .await;

        assert_eq!(stats.errors, 0);
        assert!(stream_exists(&jetstream, &session_id).await);
        assert!(nats_session_index::get_record(&index_store, &session_id)
            .await
            .expect("get record")
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
        let index_store = nats_session_index::ensure_index_bucket(&jetstream)
            .await
            .expect("index bucket");
        let session_id = unique_session_id("lease-skip");
        put_test_stream(&jetstream, &session_id).await;
        nats_session_index::put_record(&index_store, &sample_record(&session_id, 1))
            .await
            .expect("put record");
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: jetstream.clone(),
            session_id: &session_id,
            worker_id: "lease-holder".to_string(),
            generation: 1,
            config: NatsLeaseConfig::default(),
            session_index: None,
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
        assert!(nats_session_index::get_record(&index_store, &session_id)
            .await
            .expect("get record")
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
        let index_store = nats_session_index::ensure_index_bucket(&jetstream)
            .await
            .expect("index bucket");
        let session_id = unique_session_id("leader-race");
        put_test_stream(&jetstream, &session_id).await;
        nats_session_index::put_record(&index_store, &sample_record(&session_id, 1))
            .await
            .expect("put record");

        let gc_session_id = unique_gc_session_id("leader-race-gc");
        let (first, second) = tokio::join!(
            run_remote_cleanup_with_gc_id(&config, 1, &cluster, &gc_session_id),
            run_remote_cleanup_with_gc_id(&config, 1, &cluster, &gc_session_id)
        );

        assert_eq!(first.errors, 0);
        assert_eq!(second.errors, 0);
        assert!(!stream_exists(&jetstream, &session_id).await);
        assert!(nats_session_index::get_record(&index_store, &session_id)
            .await
            .expect("get record")
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
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            agents: Vec::new(),
        });
        config
    }

    async fn put_test_stream(jetstream: &async_nats::jetstream::Context, session_id: &str) {
        let stream_name = stream_name_for_session(session_id);
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
        let lease_key = NatsLeaseConfig::default().key_for_session(session_id);
        lease_store
            .entry(lease_key)
            .await
            .expect("lease entry")
            .map(|entry| entry.operation)
    }

    async fn stream_exists(jetstream: &async_nats::jetstream::Context, session_id: &str) -> bool {
        jetstream
            .get_stream(&stream_name_for_session(session_id))
            .await
            .is_ok()
    }
}
