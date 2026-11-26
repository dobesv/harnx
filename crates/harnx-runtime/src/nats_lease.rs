mod acquisition;
mod broker_clock;
mod holder_liveness;
mod lease_ttl;
mod scope;
mod slot;
use crate::config::DEFAULT_BUCKET_REPLICAS;
use crate::nats_metrics;
use crate::nats_session_metadata::SessionMetadataStore;
use anyhow::{anyhow, bail, Context, Result};
use async_nats::header::{NATS_EXPECTED_LAST_SUBJECT_SEQUENCE, NATS_MESSAGE_ID, NATS_MESSAGE_TTL};
use async_nats::jetstream::{self, context::PublishErrorKind, kv, stream};
use futures_util::StreamExt;
pub use holder_liveness::holder_reclaim_supported;
use serde::{Deserialize, Serialize};
use slot::{CreatedLease, LeaseSlot};
use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time;

pub(crate) use acquisition::{
    assess_lease, AcquisitionPolicy, HeldAge, LeaseAssessment, ReclaimReason,
};

const LEASE_BUCKET: &str = "harnx_leases";
const LEASE_KEY_PREFIX: &str = "sessions";

/// The JetStream subject a lease key lives on, the one place the mapping
/// is spelled out.
fn kv_subject(bucket_name: &str, key: &str) -> String {
    format!("$KV.{}.{}", bucket_name, key)
}
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);
pub const DEFAULT_RENEW_INTERVAL: Duration = Duration::from_secs(10);
pub const DEFAULT_TOMBSTONE_TTL: Duration = Duration::from_secs(3600);
const ACTIVITY_REFRESH_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseRecord {
    /// Original execution the holder acquired this lease for, not the session's
    /// current generation. Recorded for diagnostics; nothing reads it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    pub worker_id: String,
    pub generation: u64,
    pub acquired_at: String,
    /// Liveness identity of the process that took the lease: pid, host
    /// identity and, on Linux, process start time. Lets a later worker on
    /// the same host reclaim the lease once the holder is provably dead,
    /// instead of waiting out the TTL. Absent in records written before this
    /// existed — such holders are never treated as dead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<holder_liveness::HolderIdentity>,
}

/// Whether the holder recorded in `record` is provably dead on this host, so
/// the lease it can no longer renew may be reclaimed. `false` on any
/// uncertainty: missing identity (records from older binaries), a different
/// host, or a liveness check that can't be trusted all leave the holder
/// unproven, with no liveness reclaim — worker activation has a separate
/// broker-clock age rule (see `nats_lease::acquisition`).
pub(crate) fn holder_is_stale(record: &LeaseRecord) -> bool {
    use holder_liveness::HolderLiveness;
    record
        .holder
        .as_ref()
        .is_some_and(|holder| holder_liveness::holder_liveness(holder) == HolderLiveness::Dead)
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
    pub session_metadata: Option<SessionMetadataStore>,
}

#[derive(Debug)]
struct RenewTaskParams {
    jetstream: jetstream::Context,
    bucket: kv::Store,
    key: String,
    state: Arc<LeaseState>,
    renew_interval: Duration,
    session_id: String,
    session_metadata: Option<SessionMetadataStore>,
}

#[derive(Debug)]
pub struct NatsSessionLease {
    bucket: kv::Store,
    jetstream: jetstream::Context,
    key: String,
    acquisition_revision: u64,
    state: Arc<LeaseState>,
    renew_task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug)]
struct LeaseState {
    execution_id: Option<String>,
    worker_id: String,
    generation: u64,
    ttl: Duration,
    /// How long the marker a release leaves on the key lasts.
    marker_ttl: Duration,
    held: AtomicBool,
    fence_token: AtomicU64,
    renew_lock: Mutex<()>,
    status_tx: watch::Sender<bool>,
    confirmed_until: parking_lot::Mutex<time::Instant>,
}

impl NatsSessionLease {
    pub async fn acquire(params: NatsLeaseAcquireParams<'_>) -> Result<Option<Self>> {
        Self::acquire_for_execution(params, None).await
    }

    /// Acquire the lease and record which execution the holder took it for.
    /// A live holder's lease is never taken. A replacement waits for the TTL
    /// unless the holder is provably dead on this host, in which case the lease
    /// is reclaimed immediately.
    pub async fn acquire_for_execution(
        params: NatsLeaseAcquireParams<'_>,
        execution_id: Option<String>,
    ) -> Result<Option<Self>> {
        let key = params.config.key_for_session(params.session_id);
        Self::acquire_key(params, execution_id, key, AcquisitionPolicy::Generic).await
    }

    /// As [`Self::acquire_for_execution`], under the worker-activation
    /// policy. Besides a holder provably dead on this host, a lease that is
    /// still present on the broker past three effective TTLs of its own —
    /// judged on the broker's clock, never the worker's — is reclaimed
    /// through the same revision-checked CAS, which is what recovers a
    /// session whose dead holder's lease outlived its message TTL. The
    /// claimant's TTL must be a positive whole number of seconds, because
    /// create and renewal encode `ttl.as_secs()` on the wire and the
    /// confirmation deadline must equal the TTL actually sent.
    pub async fn acquire_for_worker_activation(
        params: NatsLeaseAcquireParams<'_>,
        execution_id: Option<String>,
    ) -> Result<Option<Self>> {
        let key = params.config.key_for_session(params.session_id);
        Self::acquire_key(
            params,
            execution_id,
            key,
            AcquisitionPolicy::WorkerActivation,
        )
        .await
    }

    async fn acquire_key(
        params: NatsLeaseAcquireParams<'_>,
        execution_id: Option<String>,
        key: String,
        policy: AcquisitionPolicy,
    ) -> Result<Option<Self>> {
        let NatsLeaseAcquireParams {
            jetstream,
            session_id,
            worker_id,
            generation,
            config,
            session_metadata,
        } = params;
        if policy == AcquisitionPolicy::WorkerActivation {
            validate_worker_ttl(config.ttl)?;
        }
        config.validate()?;
        let bucket = ensure_lease_bucket(&jetstream, &config).await?;
        let mut record = LeaseRecord::new(worker_id.clone(), generation);
        record.execution_id = execution_id.clone();
        let slot = LeaseSlot {
            jetstream: &jetstream,
            bucket: &bucket,
            key: &key,
            marker_ttl: config.tombstone_ttl,
        };
        let acquired_at = time::Instant::now();
        let Some(created) = claim_lease(&slot, session_id, &record, &config, policy).await? else {
            return Ok(None);
        };
        if policy == AcquisitionPolicy::WorkerActivation
            && discard_expired_create(&slot, session_id, &worker_id, &created, config.ttl).await
        {
            return Ok(None);
        }
        // Worker confirmations anchor to the instant immediately before the
        // successful TTL-bearing create: the clock round trips an age check
        // can add before it must not spend the lease's renewal budget, and
        // anchoring after the acknowledgement would overstate the remaining
        // TTL. Every other acquirer keeps the old anchor.
        let confirmed_from = match policy {
            AcquisitionPolicy::WorkerActivation => created.sent_at,
            AcquisitionPolicy::Generic => acquired_at,
        };
        let confirmed_until = confirmed_from + config.ttl;
        Ok(Some(finish_acquisition(FinishAcquisition {
            jetstream,
            bucket,
            key,
            session_id,
            worker_id,
            generation,
            execution_id,
            config: &config,
            session_metadata,
            created: &created,
            confirmed_until,
        })))
    }

    pub fn is_held(&self) -> bool {
        self.state.held.load(Ordering::SeqCst)
    }

    /// Perform a broker-authoritative ownership check by renewing with the
    /// lease's current KV revision. Failure marks the local lease lost so an
    /// approved tool isn't started once loss is observed; this does not
    /// atomically prevent expiry or takeover after the check returns, or revoke
    /// already in-flight tool effects.
    pub async fn revalidate_ownership(&self) -> Result<bool> {
        if !self.is_held() {
            return Ok(false);
        }
        let _renew_guard = self.state.renew_lock.lock().await;
        if !self.is_held() {
            return Ok(false);
        }
        match renew_once(&self.jetstream, &self.bucket, &self.key, &self.state).await {
            Ok(_) => Ok(true),
            Err(error) => {
                warn!(
                    "nats lease ownership revalidation failed: worker_id={} generation={} revision={} reason={error:#}",
                    self.worker_id(),
                    self.generation(),
                    self.fence_token()
                );
                if self.state.mark_lost() {
                    nats_metrics::lease_lost();
                }
                Ok(false)
            }
        }
    }

    /// The lease's current KV revision, stamped as diagnostic metadata on
    /// durable log entries that carry a `fence_token` field. Durable log
    /// appends check the local `is_held()` flag and the session log's tail
    /// sequence (`append_fenced`), not this lease revision on the broker. Every
    /// renewal rewrites the lease and raises it, so compare against the token
    /// a write actually carried rather than one read afterwards.
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
        // A renewal may already have committed in NATS while its acknowledgement
        // is still in flight. Let it save the new revision before aborting it;
        // deleting with the old revision otherwise strands the lease until TTL.
        let _renew_guard = self.state.renew_lock.lock().await;
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
        match harnx_metrics::time_nats_operation(
            "lease_release",
            self.bucket
                .purge_expect_revision_with_ttl(&self.key, revision, self.state.marker_ttl),
        )
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
        let state = Arc::clone(&self.state);
        let task = self.renew_task.get_mut().take();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // As with explicit release, let a committed renewal publish its
                // revision locally before stopping it and deleting the lease.
                let _renew_guard = state.renew_lock.lock().await;
                let held = state.mark_lost();
                if let Some(task) = task {
                    task.abort();
                    let _ = task.await;
                }
                if held {
                    let revision = state.fence_token.load(Ordering::SeqCst);
                    let _ = harnx_metrics::time_nats_operation(
                        "lease_release",
                        bucket.purge_expect_revision_with_ttl(&key, revision, state.marker_ttl),
                    )
                    .await;
                }
            });
        } else {
            state.mark_lost();
            if let Some(task) = task {
                task.abort();
            }
        }
    }
}

impl LeaseRecord {
    fn new(worker_id: String, generation: u64) -> Self {
        Self {
            execution_id: None,
            worker_id,
            generation,
            acquired_at: chrono::Utc::now().to_rfc3339(),
            holder: Some(holder_liveness::current()),
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
/// `None` means the lookup did not succeed — either no worker has ever taken a
/// lease on this cluster, or the lookup failed transiently. Callers that poll
/// should hold on to the returned store: opening it costs a `stream_info` round
/// trip that a plain `get` does not.
pub async fn open_lease_bucket(
    jetstream: &jetstream::Context,
    config: &NatsLeaseConfig,
) -> Option<kv::Store> {
    harnx_metrics::time_nats_operation("kv_bucket_open", jetstream.get_key_value(&config.bucket))
        .await
        .ok()
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
    let Some(entry) = harnx_metrics::time_nats_operation("lease_get", bucket.get(&key))
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

/// Check whether a worker currently holds the session lease without acquiring
/// or mutating it. A missing lease bucket means no session can currently be
/// leased on this cluster.
pub async fn session_has_active_lease(
    jetstream: &jetstream::Context,
    session_id: &str,
) -> Result<bool> {
    let config = NatsLeaseConfig::default();
    let bucket = match harnx_metrics::time_nats_operation(
        "kv_bucket_open",
        jetstream.get_key_value(&config.bucket),
    )
    .await
    {
        Ok(bucket) => bucket,
        Err(error) if crate::nats_admin::kv_bucket_missing(&error) => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to open NATS lease bucket '{}'", config.bucket));
        }
    };
    Ok(lease_holder_in(&bucket, &config, session_id)
        .await?
        .is_some())
}

/// Every live lease key in the default lease bucket (`harnx_leases`), read
/// through one LastPerSubject consumer the way the session metadata listing
/// reads its bucket.
///
/// A stats-only snapshot: delete and purge markers are ignored, the read
/// doesn't renew anything, and the result is never a deletion authority — a
/// lease that is live here can still expire, and one that looks gone here can
/// still be re-read live by the cleanup guards. `Ok(None)` means the bucket
/// does not exist, which no lease holder has ever needed on this cluster.
pub async fn snapshot_live_lease_keys(
    jetstream: &jetstream::Context,
) -> Result<Option<std::collections::HashSet<String>>> {
    let bucket = match jetstream.get_key_value(LEASE_BUCKET).await {
        Ok(bucket) => bucket,
        Err(error) if crate::nats_admin::kv_bucket_missing(&error) => return Ok(None),
        Err(error) => {
            return Err(anyhow::Error::from(error))
                .with_context(|| format!("Failed to open NATS lease bucket '{LEASE_BUCKET}'"));
        }
    };
    let consumer = bucket
        .stream
        .create_consumer(async_nats::jetstream::consumer::push::OrderedConfig {
            deliver_subject: jetstream.client().new_inbox(),
            description: Some("lease stats snapshot".into()),
            filter_subject: format!("{}>", bucket.prefix),
            deliver_policy: async_nats::jetstream::consumer::DeliverPolicy::LastPerSubject,
            replay_policy: async_nats::jetstream::consumer::ReplayPolicy::Instant,
            ..Default::default()
        })
        .await
        .with_context(|| format!("Failed to snapshot NATS lease bucket '{LEASE_BUCKET}'"))?;
    if consumer.cached_info().num_pending == 0 {
        return Ok(Some(std::collections::HashSet::new()));
    }
    let mut messages = consumer.messages().await?;
    let mut live = std::collections::HashSet::new();
    while let Some(message) = messages.next().await {
        let message = message?;
        let info = message.info().map_err(anyhow::Error::from_boxed)?;
        // The last delivered record can be a Delete/Purge marker left by a
        // released lease: finish on it before the operation filter, or the
        // loop would wait forever for a message a quiet bucket never sends.
        let last_delivered = info.pending == 0;
        // The same decoder the authoritative leader reads use: a revision the
        // server removed itself carries a marker reason, not a KV operation,
        // and must not count as a live lease.
        let operation = harnx_nats_common::markers::operation(message.headers.as_ref());
        if operation == kv::Operation::Put {
            let key = message
                .subject
                .strip_prefix(&bucket.prefix)
                .with_context(|| {
                    format!("Unexpected subject in lease snapshot: {}", message.subject)
                })?;
            live.insert(key.to_owned());
        }
        if last_delivered {
            return Ok(Some(live));
        }
    }
    anyhow::bail!("NATS lease snapshot ended before catching up")
}

/// Take the lease key for `record`, reclaiming it first when the observed
/// holder may be reclaimed under `policy`: provably dead on this host, or —
/// for worker activation — old on the broker's clock. `Ok(None)` means
/// another holder keeps it.
///
/// Read and broker-clock failures propagate: the calling stage settles them
/// through its existing error path, and no failure ever purges. A malformed
/// lease record stays held rather than becoming a free lease.
async fn claim_lease(
    slot: &LeaseSlot<'_>,
    session_id: &str,
    record: &LeaseRecord,
    config: &NatsLeaseConfig,
    policy: AcquisitionPolicy,
) -> Result<Option<CreatedLease>> {
    if let Some(created) = create_lease(slot, record, config.ttl).await? {
        return Ok(Some(created));
    }
    // Another holder owns the key. The assessment re-reads the entry from
    // the stream leader and re-applies the same policy against this
    // claimant's configuration, so an old observation never carries onto a
    // revision someone else may have rewritten meanwhile.
    let assessment =
        acquisition::assess_lease(slot.jetstream, slot.bucket, slot.key, config, policy)
            .await
            .with_context(|| format!("Failed to read NATS lease for session '{session_id}'"))?;
    let LeaseAssessment::Reclaimable {
        revision,
        record: held,
        reason,
    } = assessment
    else {
        // Held, gone, or undecodable: the TTL (or the next delivery) is the
        // only reclaim path left.
        return Ok(None);
    };
    if !delete_stale_lease_at_revision(slot, session_id, revision, &held, reason).await? {
        return Ok(None);
    }
    create_lease(slot, record, config.ttl).await
}

/// Claim the session's lease key. A create conflict means another owner still
/// holds it, so the caller backs off rather than taking the session over; the
/// holder's own TTL is what eventually frees the key.
async fn create_lease(
    slot: &LeaseSlot<'_>,
    record: &LeaseRecord,
    ttl: Duration,
) -> Result<Option<CreatedLease>> {
    let payload = serde_json::to_vec(record).context("Failed to serialize lease record")?;
    harnx_metrics::time_nats_operation("lease_create", slot.create(payload, ttl)).await
}

/// Inputs to [`finish_acquisition`]: everything a claimed lease needs to
/// become a live [`NatsSessionLease`].
struct FinishAcquisition<'a> {
    jetstream: jetstream::Context,
    bucket: kv::Store,
    key: String,
    session_id: &'a str,
    worker_id: String,
    generation: u64,
    execution_id: Option<String>,
    config: &'a NatsLeaseConfig,
    session_metadata: Option<SessionMetadataStore>,
    created: &'a CreatedLease,
    confirmed_until: time::Instant,
}

/// Everything after a successful claim: local state, the acquisition log,
/// the renewal task, and the handle joining them.
fn finish_acquisition(acquired: FinishAcquisition<'_>) -> NatsSessionLease {
    let FinishAcquisition {
        jetstream,
        bucket,
        key,
        session_id,
        worker_id,
        generation,
        execution_id,
        config,
        session_metadata,
        created,
        confirmed_until,
    } = acquired;
    let (status_tx, _status_rx) = watch::channel(true);
    let state = Arc::new(LeaseState {
        execution_id,
        worker_id,
        generation,
        ttl: config.ttl,
        marker_ttl: config.tombstone_ttl,
        held: AtomicBool::new(true),
        fence_token: AtomicU64::new(created.revision),
        renew_lock: Mutex::new(()),
        status_tx,
        confirmed_until: parking_lot::Mutex::new(confirmed_until),
    });
    info!(
        "nats lease acquired: session_id={session_id} worker_id={} generation={} revision={}",
        state.worker_id, state.generation, created.revision
    );
    nats_metrics::lease_acquired();

    let renew_task = spawn_renew_task(RenewTaskParams {
        jetstream: jetstream.clone(),
        bucket: bucket.clone(),
        key: key.clone(),
        state: Arc::clone(&state),
        renew_interval: config.renew_interval,
        session_id: session_id.to_string(),
        session_metadata,
    });

    NatsSessionLease {
        bucket,
        jetstream,
        key,
        acquisition_revision: created.revision,
        state,
        renew_task: Mutex::new(Some(renew_task)),
    }
}

/// Whether the broker's acknowledgement of a TTL-bearing create arrived
/// after the lease it confirmed had already expired. Exact equality with the
/// deadline still leaves the lease live: expiry is strict.
fn create_ack_expired(created: &CreatedLease, ttl: Duration) -> bool {
    created.acked_at > created.sent_at + ttl
}

/// The worker activation path requires a positive whole-second TTL: create
/// and renewal publish `ttl.as_secs()`, so a fractional or subsecond value
/// would put a longer window on the wire's confirmation deadline than the
/// expiry the broker actually enforces. Generic acquirers keep their own
/// validation.
fn validate_worker_ttl(ttl: Duration) -> Result<()> {
    if ttl.is_zero() || ttl.subsec_nanos() != 0 {
        bail!(
            "worker activation lease TTL must be a positive whole number of seconds, got {ttl:?}"
        );
    }
    Ok(())
}

/// A worker-activation create whose acknowledgement outlived the lease it
/// confirmed may already have expired by the time this worker learned it
/// owns the key, so no protected session work may run under it: purge it at
/// its own revision and report the lease unavailable so the activation
/// defers. A renewal or another claimant that moved first makes the purge
/// CAS lose, which is fine.
async fn discard_expired_create(
    slot: &LeaseSlot<'_>,
    session_id: &str,
    worker_id: &str,
    created: &CreatedLease,
    ttl: Duration,
) -> bool {
    if !create_ack_expired(created, ttl) {
        return false;
    }
    warn!(
        "nats lease create acknowledgement arrived after the TTL: session_id={session_id} worker_id={worker_id} revision={} ttl_secs={}",
        created.revision,
        ttl.as_secs()
    );
    if let Err(error) =
        harnx_metrics::time_nats_operation("lease_release", slot.remove(created.revision)).await
    {
        if error.kind() != kv::UpdateErrorKind::WrongLastRevision {
            log::warn!("failed to discard an expired-acquired lease: {error:#}");
        }
    }
    true
}

/// CAS-delete a stale lease at the revision it was observed at. `Ok(false)`
/// means the delete lost the CAS: another writer moved first and this
/// attempt gives up. Split from [`claim_lease`] so the CAS outcome can be
/// tested directly against a replaced lease.
///
/// A holder identity is optional evidence here: a claim from a foreign host
/// or an older record without one can still be reclaimed by age. The holder's
/// pid, when recorded, is logged, never required.
async fn delete_stale_lease_at_revision(
    slot: &LeaseSlot<'_>,
    session_id: &str,
    revision: u64,
    record: &LeaseRecord,
    reason: ReclaimReason,
) -> Result<bool> {
    match harnx_metrics::time_nats_operation("lease_reclaim", slot.remove(revision)).await {
        Ok(()) => {
            log_reclaimed_lease(session_id, revision, record, &reason);
            nats_metrics::lease_reclaimed();
            Ok(true)
        }
        // The holder renewed or another worker reclaimed first: not ours.
        Err(error) if error.kind() == kv::UpdateErrorKind::WrongLastRevision => {
            debug!(
                "stale lease reclaim lost the CAS for session '{session_id}'; \
                 another writer moved first"
            );
            Ok(false)
        }
        Err(error) => Err(anyhow::Error::from(error)).with_context(|| {
            format!(
                "Failed to reclaim stale NATS lease key '{}' for session '{session_id}'",
                slot.key
            )
        }),
    }
}

/// Diagnostics for a successful reclaim, per reason. Proven death and old age
/// are reported as what they are: age alone is never proof that the holder
/// process died, and its logs must not say so.
fn log_reclaimed_lease(
    session_id: &str,
    revision: u64,
    record: &LeaseRecord,
    reason: &ReclaimReason,
) {
    let holder_pid = record
        .holder
        .as_ref()
        .map(|holder| holder.pid)
        .unwrap_or_default();
    match reason {
        ReclaimReason::DeadHolder => {
            info!(
                "reclaimed stale session lease from dead holder: session_id={session_id} holder_worker_id={} holder_pid={} holder_generation={}",
                record.worker_id, holder_pid, record.generation
            );
        }
        ReclaimReason::BrokerAge { age, effective_ttl } => {
            info!(
                "reclaimed stale session lease by broker-clock age: session_id={session_id} holder_worker_id={} holder_pid={} holder_generation={} revision={revision} age_secs={} effective_ttl_secs={} reason=broker-age",
                record.worker_id,
                holder_pid,
                record.generation,
                age.as_secs(),
                effective_ttl.as_secs()
            );
        }
    }
}

/// Open the lease bucket, creating it at `config.replicas` if it doesn't
/// exist yet, or raising an existing bucket's replica count to match if an
/// operator changed cluster config after the bucket was first created. This
/// bucket coordinates single-active-worker ownership via lease-key revision
/// CAS (log writes themselves use local `is_held()` checks and session-log
/// tail CAS rather than an atomic broker-side lease check), so a bucket stuck
/// at R=1 on a cluster configured for R=3 defeats the point of running a
/// cluster at all.
pub async fn ensure_lease_bucket(
    jetstream: &jetstream::Context,
    config: &NatsLeaseConfig,
) -> Result<kv::Store> {
    let create = harnx_metrics::time_nats_operation(
        "kv_bucket_create",
        jetstream.create_key_value(kv::Config {
            bucket: config.bucket.clone(),
            history: 1,
            limit_markers: Some(config.tombstone_ttl),
            num_replicas: config.replicas,
            storage: stream::StorageType::File,
            ..Default::default()
        }),
    )
    .await;
    if let Ok(bucket) = create {
        return Ok(bucket);
    }

    if let Err(error) = harnx_nats_common::registry::reconcile_bucket_replicas(
        jetstream,
        &config.bucket,
        config.replicas,
    )
    .await
    {
        // A read-only or permission-limited connection can still use the
        // bucket; losing the intended replica count is a degradation, not a
        // failure to start.
        log::warn!(
            "could not reconcile replicas for lease bucket '{}': {error:#}",
            config.bucket
        );
    }

    harnx_metrics::time_nats_operation("kv_bucket_open", jetstream.get_key_value(&config.bucket))
        .await
        .map_err(anyhow::Error::from)
        .with_context(|| {
            format!(
                "Failed to create or open NATS lease bucket '{}'",
                config.bucket
            )
        })
}

fn spawn_renew_task(params: RenewTaskParams) -> JoinHandle<()> {
    let RenewTaskParams {
        jetstream,
        bucket,
        key,
        state,
        renew_interval,
        session_id,
        session_metadata,
    } = params;
    tokio::spawn(async move {
        let mut ticker = time::interval(renew_interval);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        let mut activity_refresh: Option<JoinHandle<()>> = None;
        loop {
            ticker.tick().await;
            if !state.held.load(Ordering::SeqCst) {
                break;
            }
            let _renew_guard = state.renew_lock.lock().await;
            if !state.held.load(Ordering::SeqCst) {
                break;
            }
            match renew_once(&jetstream, &bucket, &key, &state).await {
                Ok(new_revision) => {
                    if let Some(store) = session_metadata.clone() {
                        schedule_activity_refresh(
                            &mut activity_refresh,
                            refresh_activity(
                                store,
                                session_id.clone(),
                                state.worker_id.clone(),
                                state.generation,
                            ),
                        );
                    }
                    debug!(
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
        if let Some(refresh) = activity_refresh {
            refresh.abort();
        }
    })
}

fn schedule_activity_refresh(
    task: &mut Option<JoinHandle<()>>,
    refresh: impl Future<Output = ()> + Send + 'static,
) {
    if task.as_ref().is_some_and(JoinHandle::is_finished) {
        task.take();
    }
    if task.is_none() {
        *task = Some(tokio::spawn(refresh));
    }
}

async fn refresh_activity(
    store: SessionMetadataStore,
    session_id: String,
    worker_id: String,
    generation: u64,
) {
    match time::timeout(ACTIVITY_REFRESH_TIMEOUT, store.touch_activity(&session_id)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            warn!(
                "failed to refresh remote session activity on lease renew: session_id={session_id} worker_id={worker_id} generation={generation} err={error:#}"
            );
        }
        Err(_) => {
            warn!(
                "timed out refreshing remote session activity on lease renew: session_id={session_id} worker_id={worker_id} generation={generation} timeout_ms={}",
                ACTIVITY_REFRESH_TIMEOUT.as_millis()
            );
        }
    }
}

async fn renew_once(
    jetstream: &jetstream::Context,
    bucket: &kv::Store,
    key: &str,
    state: &LeaseState,
) -> Result<u64> {
    let expected_revision = state.fence_token.load(Ordering::SeqCst);
    let sent_at = time::Instant::now();
    let deadline = *state.confirmed_until.lock();
    let mut record = LeaseRecord::new(state.worker_id.clone(), state.generation);
    record.execution_id = state.execution_id.clone();
    let payload =
        serde_json::to_vec(&record).context("Failed to serialize renewed lease record")?;
    let subject = kv_subject(&bucket.name, key);
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(
        NATS_EXPECTED_LAST_SUBJECT_SEQUENCE,
        expected_revision.to_string(),
    );
    headers.insert(NATS_MESSAGE_TTL, state.ttl.as_secs().to_string());
    headers.insert(
        NATS_MESSAGE_ID,
        format!(
            "lease:{key}:{}:{}:{expected_revision}",
            state.worker_id, state.generation
        ),
    );
    let payload = bytes::Bytes::from(payload);
    let ack = harnx_metrics::time_nats_operation(
        "lease_renew",
        harnx_nats_common::recovery::retry_until(
            deadline,
            || async {
                jetstream
                    .publish_with_headers(subject.clone(), headers.clone(), payload.clone())
                    .await?
                    .await
            },
            harnx_nats_common::recovery::transient_publish,
        ),
    )
    .await;
    match ack {
        Ok(ack) => {
            state.fence_token.store(ack.sequence, Ordering::SeqCst);
            *state.confirmed_until.lock() = sent_at + state.ttl;
            Ok(ack.sequence)
        }
        Err(error)
            if error
                .downcast_ref::<jetstream::context::PublishError>()
                .is_some_and(|error| error.kind() == PublishErrorKind::WrongLastSequence) =>
        {
            bail!("Lost lease CAS for key '{key}'")
        }
        Err(error) => {
            Err(anyhow!(error)).with_context(|| format!("CAS renewal failed for lease key '{key}'"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn lease_record_deserializes_legacy_json_without_holder() {
        let json = r#"{"worker_id":"w1","generation":3,"acquired_at":"2024-01-01T00:00:00Z"}"#;
        let record: LeaseRecord = serde_json::from_str(json).expect("legacy record decodes");
        assert_eq!(record.worker_id, "w1");
        // Records written before holder identity existed never authorize a
        // reclaim.
        assert!(record.holder.is_none());
        assert!(!holder_is_stale(&record));
    }

    #[test]
    fn lease_record_roundtrips_holder_identity() {
        let record = LeaseRecord::new("w1".into(), 7);
        assert!(record.holder.is_some());
        let json = serde_json::to_string(&record).expect("encode record");
        let decoded: LeaseRecord = serde_json::from_str(&json).expect("decode record");
        assert_eq!(decoded, record);
        // A record this process wrote names a holder that is alive.
        assert!(!holder_is_stale(&decoded));
    }

    fn dead_holder_on_this_host() -> Option<holder_liveness::HolderIdentity> {
        if !holder_reclaim_supported() {
            // Without a strong host identity (macOS, Windows, restricted
            // procfs) reclaim stays disabled entirely.
            return None;
        }
        // Spawn and reap a child; its pid is gone from the process table, and
        // the test sandbox shares one pid namespace between test and child.
        let child = std::process::Command::new("sleep")
            .arg("0")
            .spawn()
            .expect("spawn sleep for stale-holder test");
        let pid = child.id();
        let _ = child.wait_with_output();
        Some(holder_liveness::HolderIdentity {
            pid,
            host: holder_liveness::current().host,
            start_ticks: None,
        })
    }

    /// Lease timings that let an age reclaim land inside a test: the
    /// claimant's TTL is a whole second, so the create acknowledgement
    /// deadline is tight and renewal is fast. The seeded writer's lease is
    /// headerless (a legacy record, or one whose TTL evidence a broker
    /// restart stranded), so it never expires on its own and the age
    /// threshold comes from the legacy default floor. A TTL-headed seed
    /// cannot reproduce the stranded state against a live nats-server
    /// (verified on 2.11.6): the broker enforces a Put's own `Nats-TTL`
    /// inline, so the old record reads back as a marker within seconds,
    /// never as an aged holder.
    fn fast_age_lease_config() -> NatsLeaseConfig {
        NatsLeaseConfig {
            ttl: Duration::from_secs(1),
            renew_interval: Duration::from_millis(250),
            replicas: 1,
            ..Default::default()
        }
    }

    /// Seed another worker's lease as a headerless Put: nothing on the
    /// broker ever expires it, and a live process on this host wrote it, so
    /// neither the TTL nor a dead-holder check can free it. Only the
    /// broker-clock age policy can take it back.
    async fn seed_headerless_live_lease(
        bucket: &kv::Store,
        config: &NatsLeaseConfig,
        session_id: &str,
        worker_id: &str,
    ) -> Result<LeaseRecord> {
        let record = LeaseRecord::new(worker_id.into(), 1);
        // This process is alive: no dead-holder reclaim can fire.
        assert!(!holder_is_stale(&record));
        bucket
            .create(
                &config.key_for_session(session_id),
                serde_json::to_vec(&record)?.into(),
            )
            .await?;
        Ok(record)
    }

    async fn acquire_as(
        policy_worker_activation: bool,
        js: &jetstream::Context,
        config: &NatsLeaseConfig,
        session_id: &str,
        worker_id: &str,
    ) -> Result<Option<NatsSessionLease>> {
        let params = NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id,
            worker_id: worker_id.into(),
            generation: 2,
            config: config.clone(),
            session_metadata: None,
        };
        if policy_worker_activation {
            NatsSessionLease::acquire_for_worker_activation(params, None).await
        } else {
            NatsSessionLease::acquire(params).await
        }
    }

    /// A lease with no TTL evidence — the legacy/stranded record shape —
    /// must be reclaimed by the worker-activation policy once it is past
    /// three effective TTLs of age on the broker's own clock, where the
    /// effective TTL is the legacy default floor.
    #[tokio::test]
    async fn worker_activation_reclaims_an_old_headerless_lease() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = fast_age_lease_config();
        let bucket = ensure_lease_bucket(&js, &config).await?;

        seed_headerless_live_lease(&bucket, &config, "age-reclaim", "stranded-worker").await?;
        // Three effective TTLs at the 30s legacy floor is 90s, and
        // eligibility is strictly greater: sleep past it with margin.
        tokio::time::sleep(Duration::from_secs(92)).await;

        let reclaims_before = nats_metrics::snapshot().lease_reclaims;
        let lease = acquire_as(true, &js, &config, "age-reclaim", "new-worker")
            .await?
            .expect("a headerless lease past three floor TTLs must be reclaimed");
        assert!(lease.is_held());
        assert!(
            nats_metrics::snapshot().lease_reclaims > reclaims_before,
            "the age reclaim must be counted"
        );
        // The reclaim was by age, not a proven death: the claimant's own
        // lease must be a TTL-bearing create over the purge marker, so the
        // session can never strand again.
        let latest = bucket
            .stream
            .get_last_raw_message_by_subject(&kv_subject(&bucket.name, lease.key()))
            .await?;
        assert_eq!(
            latest
                .headers
                .get(async_nats::header::NATS_MESSAGE_TTL)
                .map(|ttl| ttl.as_str().to_owned()),
            Some(config.ttl.as_secs().to_string())
        );
        lease.stop_renewal_for_test().await;
        drop(lease);
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn worker_activation_keeps_deferring_below_the_age_threshold() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = fast_age_lease_config();
        let bucket = ensure_lease_bucket(&js, &config).await?;

        seed_headerless_live_lease(&bucket, &config, "age-young", "slow-worker").await?;
        // Two seconds is under the 90s floor threshold: age eligibility
        // is strict, and a young lease is the holder's no matter what the
        // claimant configured.
        tokio::time::sleep(Duration::from_secs(2)).await;

        let reclaims_before = nats_metrics::snapshot().lease_reclaims;
        let lease = acquire_as(true, &js, &config, "age-young", "new-worker").await?;
        assert!(
            lease.is_none(),
            "a lease under three effective TTLs of age must not be reclaimed"
        );
        assert_eq!(
            nats_metrics::snapshot().lease_reclaims,
            reclaims_before,
            "no reclaim may happen below the threshold"
        );
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn generic_acquisition_never_reclaims_by_age() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = fast_age_lease_config();
        let bucket = ensure_lease_bucket(&js, &config).await?;

        seed_headerless_live_lease(&bucket, &config, "age-generic", "stranded-worker").await?;
        tokio::time::sleep(Duration::from_secs(92)).await;

        let reclaims_before = nats_metrics::snapshot().lease_reclaims;
        let lease = acquire_as(false, &js, &config, "age-generic", "gc-election").await?;
        assert!(
            lease.is_none(),
            "generic acquisitions must keep deferring on a stale lease"
        );
        assert_eq!(
            nats_metrics::snapshot().lease_reclaims,
            reclaims_before,
            "generic acquisitions must not reclaim by age"
        );
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    /// A TTL-bearing lease whose expiry the broker actually enforced leaves
    /// a marker, not a holder. The claim must still succeed — over the
    /// marker, with a TTL-bearing create — without any reclaim.
    #[tokio::test]
    async fn worker_activation_claims_over_an_expiry_marker() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = fast_age_lease_config();
        let bucket = ensure_lease_bucket(&js, &config).await?;

        let record = LeaseRecord::new("dead-worker".into(), 1);
        assert!(!holder_is_stale(&record));
        bucket
            .create_with_ttl(
                &config.key_for_session("age-marker"),
                serde_json::to_vec(&record)?.into(),
                config.ttl,
            )
            .await?;
        // Let the broker expire the message and place its marker.
        tokio::time::sleep(Duration::from_secs(4)).await;

        let reclaims_before = nats_metrics::snapshot().lease_reclaims;
        let lease = acquire_as(true, &js, &config, "age-marker", "new-worker")
            .await?
            .expect("a lease the broker expired must be claimable over its marker");
        assert!(lease.is_held());
        let latest = bucket
            .stream
            .get_last_raw_message_by_subject(&kv_subject(&bucket.name, lease.key()))
            .await?;
        assert_eq!(
            latest
                .headers
                .get(async_nats::header::NATS_MESSAGE_TTL)
                .map(|ttl| ttl.as_str().to_owned()),
            Some(config.ttl.as_secs().to_string()),
            "the claim must leave a TTL-bearing lease, not a marker"
        );
        assert_eq!(
            nats_metrics::snapshot().lease_reclaims,
            reclaims_before,
            "claiming over an expiry marker is not a reclaim"
        );
        lease.stop_renewal_for_test().await;
        drop(lease);
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    /// A healthy holder renewing faster than its TTL always looks young on
    /// the broker's clock: age reclaim must defer to it, and once it
    /// releases, the next activation claim must succeed normally.
    #[tokio::test]
    async fn a_renewing_holder_is_protected_from_age_reclaim() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = fast_age_lease_config();
        // Longer than the test run, so the lease never expires on its own.
        let holder_config = NatsLeaseConfig {
            ttl: Duration::from_secs(1),
            renew_interval: Duration::from_millis(200),
            replicas: 1,
            ..Default::default()
        };
        let holder = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: "age-renewing",
            worker_id: "renewing-worker".into(),
            generation: 1,
            config: holder_config.clone(),
            session_metadata: None,
        })
        .await?
        .expect("the healthy holder takes the lease");
        assert!(holder.is_held());

        // The renewal cadence keeps the observed message young; several
        // attempts all see a fresh lease and defer.
        for _ in 0..3 {
            let lease = acquire_as(true, &js, &config, "age-renewing", "new-worker").await?;
            assert!(
                lease.is_none(),
                "a renewing holder's lease must never be reclaimed by age"
            );
        }
        holder.release().await?;
        let lease = acquire_as(true, &js, &config, "age-renewing", "new-worker")
            .await?
            .expect("the released lease is claimable");
        lease.stop_renewal_for_test().await;
        drop(lease);
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn worker_activation_requires_a_positive_whole_second_ttl() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        for ttl in [
            Duration::from_millis(1500),
            Duration::from_millis(500),
            Duration::ZERO,
        ] {
            let config = NatsLeaseConfig {
                ttl,
                renew_interval: Duration::from_millis(100),
                replicas: 1,
                ..Default::default()
            };
            let error = acquire_as(true, &js, &config, "age-ttl-invalid", "new-worker")
                .await
                .expect_err("a fractional or zero TTL must fail the worker activation claim");
            assert!(
                error
                    .to_string()
                    .contains("positive whole number of seconds"),
                "unexpected error: {error:#}"
            );
        }
        // Generic acquisitions keep their own, laxer rules: no worker
        // validation fires, and whatever happens next is the generic
        // path's existing behavior — here the broker's own TTL rules
        // rejecting a subsecond TTL encoded as zero seconds.
        let config = NatsLeaseConfig {
            ttl: Duration::from_millis(500),
            renew_interval: Duration::from_millis(100),
            replicas: 1,
            ..Default::default()
        };
        let error = acquire_as(false, &js, &config, "age-ttl-invalid", "new-worker")
            .await
            .expect_err("a subsecond TTL is rejected by the broker's own rules");
        let rendered = format!("{error:#}");
        assert!(
            !rendered.contains("positive whole number of seconds"),
            "worker-only validation must not fire on the generic path: {rendered}"
        );
        assert!(
            rendered.contains("invalid per-message TTL"),
            "unexpected error: {rendered}"
        );
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    /// A create whose acknowledgement lands after the TTL it confirmed may
    /// already have expired: no session work may run under it. The
    /// acquisition must purge it at its own revision and report the lease
    /// unavailable, and the next claim must succeed cleanly.
    #[tokio::test]
    async fn an_expired_create_acknowledgement_never_protects_work() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = fast_age_lease_config();

        slot::set_create_ack_delay_for_test(Duration::from_millis(1500));
        let lease = acquire_as(true, &js, &config, "age-ack-expired", "new-worker").await?;
        assert!(
            lease.is_none(),
            "an acknowledgement past the TTL must not yield a usable lease"
        );
        assert!(
            !session_has_active_lease(&js, "age-ack-expired").await?,
            "the expired create must have been purged at its own revision"
        );
        slot::clear_create_ack_delay_for_test();

        let lease = acquire_as(true, &js, &config, "age-ack-expired", "new-worker")
            .await?
            .expect("a clean claim succeeds after the delayed create was discarded");
        assert!(lease.is_held());
        // The confirmation deadline anchors to immediately before the
        // successful create, so a renewal before it must still be inside
        // the lease's window: a renewal now commits at a fresh revision.
        assert!(lease.revalidate_ownership().await?);
        lease.stop_renewal_for_test().await;
        drop(lease);
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn acquire_reclaims_lease_from_dead_holder() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = NatsLeaseConfig::default();
        let bucket = ensure_lease_bucket(&js, &config).await?;

        let Some(dead_holder) = dead_holder_on_this_host() else {
            // Liveness is not provable on this platform or sandbox: reclaim
            // must stay disabled there.
            let _ = nats.kill();
            let _ = nats.wait();
            return Ok(());
        };
        let mut stale = LeaseRecord::new("dead-worker".into(), 1);
        stale.holder = Some(dead_holder);
        // The nextest sandbox proves liveness on Linux; on any other strong
        // identity the precondition still holds, so assert it rather than
        // silently testing nothing.
        assert!(
            holder_is_stale(&stale),
            "identity should be provably dead on a strong host"
        );
        let reclaims_before = nats_metrics::snapshot().lease_reclaims;
        let payload = serde_json::to_vec(&stale)?.into();
        bucket
            .create_with_ttl(&config.key_for_session("reclaim-dead"), payload, config.ttl)
            .await?;

        // The stale holder must not block acquisition; the acquire path CAS
        // deletes the dead holder's lease and retries the create.
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: "reclaim-dead",
            worker_id: "new-worker".into(),
            generation: 2,
            config: config.clone(),
            session_metadata: None,
        })
        .await?
        .expect("lease acquired despite dead holder");
        assert!(lease.is_held());
        assert!(
            nats_metrics::snapshot().lease_reclaims > reclaims_before,
            "reclaim must be counted"
        );
        // The reclaim leaves a marker, and the lease taken over it still expires.
        let latest = bucket
            .stream
            .get_last_raw_message_by_subject(&kv_subject(&bucket.name, lease.key()))
            .await?;
        assert_eq!(
            latest
                .headers
                .get(async_nats::header::NATS_MESSAGE_TTL)
                .map(|ttl| ttl.as_str().to_owned()),
            Some(config.ttl.as_secs().to_string())
        );
        lease.stop_renewal_for_test().await;
        drop(lease);
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn scoped_acquisition_reclaims_a_dead_holder_on_its_own_key() -> Result<()> {
        let config = NatsLeaseConfig::default();
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let bucket = ensure_lease_bucket(&js, &config).await?;

        let Some(dead_holder) = dead_holder_on_this_host() else {
            // Liveness is not provable on this platform or sandbox: reclaim
            // must stay disabled there.
            let _ = nats.kill();
            let _ = nats.wait();
            return Ok(());
        };
        // A live session lock for the same session: a scoped claim must
        // assess its own key, and a session lock held by someone else must
        // neither block nor be purged by it. This is the A2A context
        // takeover shape — a killed replica's scoped lease beside a live
        // worker's session lock.
        let live = LeaseRecord::new("live-worker".into(), 1);
        bucket
            .create_with_ttl(
                &config.key_for_session("reclaim-scope"),
                serde_json::to_vec(&live)?.into(),
                config.ttl,
            )
            .await?;
        let mut stale = LeaseRecord::new("dead-worker".into(), 1);
        stale.holder = Some(dead_holder);
        assert!(
            holder_is_stale(&stale),
            "identity should be provably dead on a strong host"
        );
        let scope_key = config.key_for_scope("reclaim-scope", "a2a")?;
        bucket
            .create_with_ttl(&scope_key, serde_json::to_vec(&stale)?.into(), config.ttl)
            .await?;

        let reclaims_before = nats_metrics::snapshot().lease_reclaims;
        let lease = NatsSessionLease::acquire_scoped(
            NatsLeaseAcquireParams {
                jetstream: js.clone(),
                session_id: "reclaim-scope",
                worker_id: "new-worker".into(),
                generation: 2,
                config: config.clone(),
                session_metadata: None,
            },
            "a2a",
        )
        .await?
        .expect("scoped lease acquired despite dead holder");
        assert_eq!(lease.key(), scope_key);
        assert!(
            nats_metrics::snapshot().lease_reclaims > reclaims_before,
            "scoped reclaim must be counted"
        );
        // The live session lock was never the scoped claim's to judge, and
        // its holder keeps it.
        let holder = lease_holder_in(&bucket, &config, "reclaim-scope")
            .await?
            .expect("session lock still held");
        assert_eq!(holder.worker_id, "live-worker");
        lease.stop_renewal_for_test().await;
        drop(lease);
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn reclaim_delete_loses_the_cas_to_a_replacement_lease() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = NatsLeaseConfig::default();
        let bucket = ensure_lease_bucket(&js, &config).await?;

        let Some(dead_holder) = dead_holder_on_this_host() else {
            // Liveness is not provable on this platform or sandbox: reclaim
            // stays disabled there.
            let _ = nats.kill();
            let _ = nats.wait();
            return Ok(());
        };
        let key = config.key_for_session("reclaim-cas-lost");
        let mut stale = LeaseRecord::new("dead-worker".into(), 1);
        stale.holder = Some(dead_holder);
        assert!(
            holder_is_stale(&stale),
            "identity should be provably dead on a strong host"
        );
        let observed = bucket
            .create_with_ttl(&key, serde_json::to_vec(&stale)?.into(), config.ttl)
            .await?;

        // Another worker replaces the dead holder's lease after it was read:
        // the reclaim delete is CAS'd on the observed revision and must lose
        // to it, leaving the replacement untouched.
        let live = LeaseRecord::new("live-worker".into(), 2);
        bucket
            .update(&key, serde_json::to_vec(&live)?.into(), observed)
            .await
            .context("replace the lease between read and reclaim")?;

        let slot = LeaseSlot {
            jetstream: &js,
            bucket: &bucket,
            key: &key,
            marker_ttl: config.tombstone_ttl,
        };
        let reclaimed = delete_stale_lease_at_revision(
            &slot,
            "reclaim-cas-lost",
            observed,
            &stale,
            ReclaimReason::DeadHolder,
        )
        .await?;
        assert!(!reclaimed, "a replaced lease must not be reclaimed");
        let current = bucket
            .entry(&key)
            .await?
            .expect("replacement lease remains");
        let record: LeaseRecord = serde_json::from_slice(&current.value)?;
        assert_eq!(record.worker_id, "live-worker");

        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn acquire_defers_when_holder_is_alive() -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let config = NatsLeaseConfig::default();
        let bucket = ensure_lease_bucket(&js, &config).await?;

        // This very process holds the lease, so it is as alive as it gets.
        let live = LeaseRecord::new("live-worker".into(), 1);
        let payload = serde_json::to_vec(&live)?.into();
        bucket
            .create_with_ttl(&config.key_for_session("reclaim-live"), payload, config.ttl)
            .await?;

        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: "reclaim-live",
            worker_id: "new-worker".into(),
            generation: 2,
            config: config.clone(),
            session_metadata: None,
        })
        .await?;
        assert!(
            lease.is_none(),
            "a live holder's lease must never be reclaimed"
        );
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn release_waits_for_committed_renewal_revision() -> Result<()> {
        check_release_after_renewal(false).await
    }

    #[tokio::test]
    async fn drop_waits_for_committed_renewal_revision() -> Result<()> {
        check_release_after_renewal(true).await
    }

    async fn check_release_after_renewal(drop_lease: bool) -> Result<()> {
        let (url, mut nats, _store) = crate::nats_worker::tests::spawn_test_nats()
            .await
            .context("nats-server required")?;
        let js = jetstream::new(async_nats::connect(&url).await?);
        let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
            jetstream: js.clone(),
            session_id: "release-race",
            worker_id: "old-worker".into(),
            generation: 1,
            config: Default::default(),
            session_metadata: None,
        })
        .await?
        .context("lease")?;
        let state = Arc::clone(&lease.state);
        let renew_guard = state.renew_lock.lock().await;
        lease.stop_renewal_for_test().await;
        // Pause a renewal after its broker commit, before the local revision
        // update. Release must wait rather than delete using the old revision.
        let value = lease.bucket.get(&lease.key).await?.context("lease entry")?;
        let revision = lease
            .bucket
            .update(&lease.key, value, lease.fence_token())
            .await?;
        if drop_lease {
            drop(lease);
            tokio::task::yield_now().await;
            assert!(state.held.load(Ordering::SeqCst));
            state.fence_token.store(revision, Ordering::SeqCst);
            drop(renew_guard);
            tokio::time::timeout(Duration::from_secs(5), async {
                while session_has_active_lease(&js, "release-race").await.unwrap() {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
        } else {
            let release = lease.release();
            tokio::pin!(release);
            assert!(futures_util::poll!(&mut release).is_pending());
            state.fence_token.store(revision, Ordering::SeqCst);
            drop(renew_guard);
            release.await?;
        }
        assert!(!session_has_active_lease(&js, "release-race").await?);
        let _ = nats.kill();
        let _ = nats.wait();
        Ok(())
    }

    #[tokio::test]
    async fn best_effort_activity_refresh_timeout_is_bounded() {
        let never_returns = async {
            futures_util::future::pending::<()>().await;
        };

        let result = time::timeout(ACTIVITY_REFRESH_TIMEOUT, never_returns).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stalled_activity_refresh_does_not_block_renewal_cadence() {
        let started = Arc::new(AtomicUsize::new(0));
        let mut task = None;

        for _ in 0..3 {
            let started = Arc::clone(&started);
            schedule_activity_refresh(&mut task, async move {
                started.fetch_add(1, Ordering::SeqCst);
                futures_util::future::pending::<()>().await;
            });
            time::timeout(
                Duration::from_millis(50),
                time::sleep(Duration::from_millis(5)),
            )
            .await
            .expect("renewal cadence must continue while activity refresh stalls");
        }

        assert_eq!(started.load(Ordering::SeqCst), 1);
        task.expect("refresh task exists").abort();
    }
}
