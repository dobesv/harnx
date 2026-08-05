use crate::nats_metrics;
use crate::nats_session_index::{self, update_record_with_revision, SessionIndexRecord};
use anyhow::{anyhow, bail, Context, Result};
use async_nats::header::{NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, NATS_MESSAGE_TTL};
use async_nats::jetstream::{self, context::PublishErrorKind, kv, stream};
use serde::{Deserialize, Serialize};
use std::cmp;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time;

const LEASE_BUCKET: &str = "harnx_leases";
const LEASE_KEY_PREFIX: &str = "sessions";
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);
pub const DEFAULT_RENEW_INTERVAL: Duration = Duration::from_secs(10);
pub const DEFAULT_BUCKET_REPLICAS: usize = 1;
pub const DEFAULT_TOMBSTONE_TTL: Duration = Duration::from_secs(3600);
const INDEX_REFRESH_RETRY_LIMIT: usize = 3;
const INDEX_REFRESH_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseRecord {
    pub worker_id: String,
    pub generation: u64,
    pub acquired_at: String,
}

#[derive(Debug, Clone)]
pub struct NatsLeaseConfig {
    pub bucket: String,
    pub ttl: Duration,
    pub renew_interval: Duration,
    pub replicas: usize,
    pub tombstone_ttl: Duration,
}

impl Default for NatsLeaseConfig {
    fn default() -> Self {
        Self {
            bucket: LEASE_BUCKET.to_string(),
            ttl: DEFAULT_LEASE_TTL,
            renew_interval: DEFAULT_RENEW_INTERVAL,
            replicas: DEFAULT_BUCKET_REPLICAS,
            tombstone_ttl: DEFAULT_TOMBSTONE_TTL,
        }
    }
}

impl NatsLeaseConfig {
    pub fn key_for_session(&self, session_id: &str) -> String {
        format!("{LEASE_KEY_PREFIX}/{session_id}/lock")
    }
}

#[derive(Debug, Clone)]
pub struct NatsLeaseAcquireParams<'a> {
    pub jetstream: jetstream::Context,
    pub session_id: &'a str,
    pub worker_id: String,
    pub generation: u64,
    pub config: NatsLeaseConfig,
    pub session_index: Option<kv::Store>,
}

#[derive(Debug)]
struct RenewTaskParams {
    jetstream: jetstream::Context,
    bucket: kv::Store,
    key: String,
    state: Arc<LeaseState>,
    renew_interval: Duration,
    session_id: String,
    session_index: Option<kv::Store>,
}

#[derive(Debug)]
pub struct NatsSessionLease {
    bucket: kv::Store,
    jetstream: jetstream::Context,
    key: String,
    state: Arc<LeaseState>,
    renew_task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug)]
struct LeaseState {
    worker_id: String,
    generation: u64,
    ttl: Duration,
    held: AtomicBool,
    fence_token: AtomicU64,
    status_tx: watch::Sender<bool>,
}

impl NatsSessionLease {
    pub async fn acquire(params: NatsLeaseAcquireParams<'_>) -> Result<Option<Self>> {
        let NatsLeaseAcquireParams {
            jetstream,
            session_id,
            worker_id,
            generation,
            config,
            session_index,
        } = params;
        config.validate()?;
        let bucket = ensure_lease_bucket(&jetstream, &config).await?;
        let key = config.key_for_session(session_id);
        let record = LeaseRecord::new(worker_id.clone(), generation);
        let payload = serde_json::to_vec(&record).context("Failed to serialize lease record")?;

        let revision = match bucket
            .create_with_ttl(&key, payload.into(), config.ttl)
            .await
        {
            Ok(revision) => revision,
            Err(error) if is_create_conflict(&error) => return Ok(None),
            Err(error) => {
                return Err(anyhow!(error)).with_context(|| {
                    format!("Failed to acquire NATS lease for session '{session_id}'")
                })
            }
        };

        let (status_tx, _status_rx) = watch::channel(true);
        let state = Arc::new(LeaseState {
            worker_id,
            generation,
            ttl: config.ttl,
            held: AtomicBool::new(true),
            fence_token: AtomicU64::new(revision),
            status_tx,
        });
        info!(
            "nats lease acquired: session_id={session_id} worker_id={} generation={} revision={revision}",
            state.worker_id,
            state.generation
        );
        nats_metrics::lease_acquired();

        let renew_task = spawn_renew_task(RenewTaskParams {
            jetstream: jetstream.clone(),
            bucket: bucket.clone(),
            key: key.clone(),
            state: Arc::clone(&state),
            renew_interval: config.renew_interval,
            session_id: session_id.to_string(),
            session_index,
        });

        Ok(Some(Self {
            bucket,
            jetstream,
            key,
            state,
            renew_task: Mutex::new(Some(renew_task)),
        }))
    }

    pub fn is_held(&self) -> bool {
        self.state.held.load(Ordering::SeqCst)
    }

    pub fn fence_token(&self) -> u64 {
        self.state.fence_token.load(Ordering::SeqCst)
    }

    pub fn lost_watch(&self) -> watch::Receiver<bool> {
        self.state.status_tx.subscribe()
    }

    pub fn worker_id(&self) -> &str {
        &self.state.worker_id
    }

    pub fn generation(&self) -> u64 {
        self.state.generation
    }

    pub async fn release(&self) -> Result<()> {
        let handle = self.stop_renew_task().await;
        let held = self.state.mark_lost();

        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }

        if !held {
            return Ok(());
        }

        let revision = self.fence_token();
        match self
            .bucket
            .delete_expect_revision(&self.key, Some(revision))
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(()),
            Err(error) => Err(anyhow::Error::from(error))
                .with_context(|| format!("Failed to release NATS lease key '{}'", self.key)),
        }
    }

    pub async fn stop_renewal_for_test(&self) {
        if let Some(handle) = self.stop_renew_task().await {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Test helper: simulate detected lease loss. Flips `is_held()` to false
    /// and fires the lost-watch, exactly as a failed renewal would.
    pub fn mark_lost_for_test(&self) {
        if self.state.mark_lost() {
            nats_metrics::lease_lost();
        }
    }

    async fn stop_renew_task(&self) -> Option<JoinHandle<()>> {
        self.renew_task.lock().await.take()
    }
}

impl Drop for NatsSessionLease {
    fn drop(&mut self) {
        let bucket = self.bucket.clone();
        let _jetstream = self.jetstream.clone();
        let key = self.key.clone();
        let revision = self.fence_token();
        let held = self.state.mark_lost();
        let state = Arc::clone(&self.state);

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if let Some(task) = self.renew_task.get_mut().take() {
                task.abort();
            }
            if held {
                handle.spawn(async move {
                    let _ = bucket.delete_expect_revision(&key, Some(revision)).await;
                    drop(state);
                });
            }
        }
    }
}

impl LeaseRecord {
    fn new(worker_id: String, generation: u64) -> Self {
        Self {
            worker_id,
            generation,
            acquired_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

impl NatsLeaseConfig {
    fn validate(&self) -> Result<()> {
        if self.ttl.is_zero() {
            bail!("NATS lease TTL must be > 0")
        }
        if self.renew_interval.is_zero() {
            bail!("NATS lease renew interval must be > 0")
        }
        if self.renew_interval >= self.ttl {
            bail!(
                "NATS lease renew interval ({:?}) must be smaller than TTL ({:?})",
                self.renew_interval,
                self.ttl
            )
        }
        if self.replicas == 0 {
            bail!("NATS lease replica count must be >= 1")
        }
        if self.tombstone_ttl.is_zero() {
            bail!("NATS lease tombstone TTL must be > 0")
        }
        Ok(())
    }
}

impl LeaseState {
    fn mark_lost(&self) -> bool {
        let was_held = self.held.swap(false, Ordering::SeqCst);
        if was_held {
            let _ = self.status_tx.send(false);
        }
        was_held
    }
}

/// Open the lease bucket read-only, without creating it.
///
/// `Ok(None)` means no worker has ever taken a lease on this cluster. Callers
/// that poll should hold on to the returned store: opening it costs a
/// `stream_info` round trip that a plain `get` does not.
pub async fn open_lease_bucket(
    jetstream: &jetstream::Context,
    config: &NatsLeaseConfig,
) -> Option<kv::Store> {
    jetstream.get_key_value(&config.bucket).await.ok()
}

/// Read the worker currently holding a session's lease, without acquiring it.
///
/// `Ok(None)` means nobody holds the session: the key expired because its
/// holder stopped renewing, or it was released. Clients use this to tell "a
/// worker is still working" from "the worker is gone".
pub async fn lease_holder_in(
    bucket: &kv::Store,
    config: &NatsLeaseConfig,
    session_id: &str,
) -> Result<Option<LeaseRecord>> {
    let key = config.key_for_session(session_id);
    let Some(entry) = bucket
        .get(&key)
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| format!("Failed to read NATS lease for session '{session_id}'"))?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&entry)
        .map(Some)
        .with_context(|| format!("Failed to decode NATS lease record for session '{session_id}'"))
}

async fn ensure_lease_bucket(
    jetstream: &jetstream::Context,
    config: &NatsLeaseConfig,
) -> Result<kv::Store> {
    match jetstream
        .create_key_value(kv::Config {
            bucket: config.bucket.clone(),
            history: 1,
            limit_markers: Some(config.tombstone_ttl),
            num_replicas: config.replicas,
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await
    {
        Ok(bucket) => Ok(bucket),
        Err(_) => jetstream
            .get_key_value(&config.bucket)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "Failed to create or open NATS lease bucket '{}'",
                    config.bucket
                )
            }),
    }
}

fn spawn_renew_task(params: RenewTaskParams) -> JoinHandle<()> {
    let RenewTaskParams {
        jetstream,
        bucket,
        key,
        state,
        renew_interval,
        session_id,
        session_index,
    } = params;
    tokio::spawn(async move {
        let mut ticker = time::interval(renew_interval);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if !state.held.load(Ordering::SeqCst) {
                break;
            }
            match renew_once(&jetstream, &bucket, &key, &state).await {
                Ok(new_revision) => {
                    if let Some(store) = session_index.as_ref() {
                        match time::timeout(
                            INDEX_REFRESH_TIMEOUT,
                            refresh_session_index_last_activity(store, &session_id),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                warn!(
                                    "failed to refresh remote session index on lease renew: session_id={} worker_id={} generation={} err={error:#}",
                                    session_id,
                                    state.worker_id,
                                    state.generation
                                );
                            }
                            Err(_) => {
                                warn!(
                                    "timed out refreshing remote session index on lease renew: session_id={} worker_id={} generation={} timeout_ms={}",
                                    session_id,
                                    state.worker_id,
                                    state.generation,
                                    INDEX_REFRESH_TIMEOUT.as_millis()
                                );
                            }
                        }
                    }
                    info!(
                        "nats lease renewed: session_id={} worker_id={} generation={} revision={new_revision}",
                        session_id,
                        state.worker_id,
                        state.generation
                    );
                }
                Err(error) => {
                    warn!(
                        "nats lease lost: session_id={} worker_id={} generation={} revision={} reason={error:#}",
                        session_id,
                        state.worker_id,
                        state.generation,
                        state.fence_token.load(Ordering::SeqCst)
                    );
                    if state.mark_lost() {
                        nats_metrics::lease_lost();
                    }
                    break;
                }
            }
        }
    })
}

async fn renew_once(
    jetstream: &jetstream::Context,
    bucket: &kv::Store,
    key: &str,
    state: &LeaseState,
) -> Result<u64> {
    let expected_revision = state.fence_token.load(Ordering::SeqCst);
    let record = LeaseRecord::new(state.worker_id.clone(), state.generation);
    let payload =
        serde_json::to_vec(&record).context("Failed to serialize renewed lease record")?;
    let subject = format!("$KV.{}.{}", bucket.name, key);
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(
        NATS_EXPECTED_LAST_SUBJECT_SEQUENCE,
        expected_revision.to_string(),
    );
    headers.insert(NATS_MESSAGE_TTL, state.ttl.as_secs().to_string());
    let ack = jetstream
        .publish_with_headers(subject, headers, payload.into())
        .await
        .with_context(|| format!("Failed to publish renewal for lease key '{key}'"))?
        .await;
    match ack {
        Ok(ack) => {
            state.fence_token.store(ack.sequence, Ordering::SeqCst);
            Ok(ack.sequence)
        }
        Err(error) if error.kind() == PublishErrorKind::WrongLastSequence => {
            bail!("Lost lease CAS for key '{key}'")
        }
        Err(error) => {
            Err(anyhow!(error)).with_context(|| format!("CAS renewal failed for lease key '{key}'"))
        }
    }
}

async fn refresh_session_index_last_activity(store: &kv::Store, session_id: &str) -> Result<()> {
    for attempt in 0..INDEX_REFRESH_RETRY_LIMIT {
        let Some((record, revision)) =
            nats_session_index::get_record_with_revision(store, session_id).await?
        else {
            return Ok(());
        };

        let updated_record = with_refreshed_last_activity(record)?;
        match update_record_with_revision(store, &updated_record, revision).await {
            Ok(_) => return Ok(()),
            Err(error)
                if is_wrong_last_revision(&error) && attempt + 1 < INDEX_REFRESH_RETRY_LIMIT =>
            {
                continue;
            }
            Err(error) if is_wrong_last_revision(&error) => return Ok(()),
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

fn with_refreshed_last_activity(mut record: SessionIndexRecord) -> Result<SessionIndexRecord> {
    record.last_activity = cmp::max(
        record.last_activity.saturating_add(1),
        unix_timestamp_now()?,
    );
    Ok(record)
}

fn unix_timestamp_now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_secs())
}

fn is_wrong_last_revision(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<kv::UpdateError>())
        .is_some_and(|error| error.kind() == kv::UpdateErrorKind::WrongLastRevision)
}

fn is_create_conflict(error: &kv::CreateError) -> bool {
    matches!(error.kind(), kv::CreateErrorKind::AlreadyExists)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_refreshed_last_activity_saturates_on_overflow() {
        let record = SessionIndexRecord {
            session_id: "overflow-session".to_string(),
            agent_name: "hephaestus".to_string(),
            working_dir: None,
            git_branch: None,
            git_remote: None,
            title: None,
            last_activity: u64::MAX,
        };

        let refreshed = with_refreshed_last_activity(record).expect("refresh should not panic");

        assert_eq!(refreshed.last_activity, u64::MAX);
    }

    #[test]
    fn is_wrong_last_revision_detects_wrapped_update_error() {
        let wrapped_error = Err::<(), _>(anyhow::Error::from(kv::UpdateError::new(
            kv::UpdateErrorKind::WrongLastRevision,
        )))
        .with_context(|| {
            "Failed to CAS-update session index record for key 'sessions/test/meta' at revision 7"
        })
        .expect_err("wrong-last-revision should remain an error");

        assert!(is_wrong_last_revision(&wrapped_error));
        assert!(!is_wrong_last_revision(&anyhow::anyhow!("unrelated error")));
    }

    #[tokio::test]
    async fn best_effort_index_refresh_times_out_without_blocking_long() {
        let never_returns = async {
            let store = futures_util::future::pending::<kv::Store>().await;
            refresh_session_index_last_activity(&store, "session-timeout").await
        };

        let result = time::timeout(INDEX_REFRESH_TIMEOUT, never_returns).await;

        assert!(
            result.is_err(),
            "refresh_session_index_last_activity path should time out when store acquisition never completes"
        );
    }
}
