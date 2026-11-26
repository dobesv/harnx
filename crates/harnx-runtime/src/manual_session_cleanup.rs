//! Manual bounded expired-session cleanup for `harnx delete expired-sessions`
//! (#2923): a CLI-only pass, separate from the worker's periodic cleanup.
//!
//! This entry point owns the `session_metadata_gc` lease for the whole pass:
//! acquisition, renewal monitoring, live revalidation before every new
//! admission, and release on every path. It never runs the orphan-stream
//! pass and never writes the epoch-hour marker; the worker wrappers in
//! [`crate::remote_session_cleanup`] keep their exact behavior.
//!
//! Safety contract: candidates keep the revision the snapshot listed them
//! under; fresh activity, live leases and A2A admission holds (through the
//! shared read-only predicate) are pre-mutation skips that mutate nothing
//! and consume no budget. `max` bounds admissions into the mutating
//! cascade — including late skips, partial failures and fence refusals —
//! so `--max 1` can never touch a second session.
//!
//! Dry-run performs the same read-only guards and preview bounds, mutating
//! nothing but the GC lease helper's own coordination key (the sole
//! documented exception).
use crate::config::Config;
use crate::nats_lease::NatsSessionLease;
use crate::nats_session_metadata::{SessionMetadataStore, SESSION_METADATA_BUCKET};
use crate::remote_session_cleanup::{
    acquire_session_gc_lease, cleanup_threshold, load_optional_lease_store, now_unix_secs,
};
use anyhow::{Context as _, Result};
use candidate::CandidateProcessor;
use futures_util::future::BoxFuture;
use report::{select_candidates, CandidateRun};
use std::sync::Arc;

mod candidate;
mod report;
pub use report::{
    ManualCleanupMode, ManualCleanupOptions, ManualCleanupOutcome, ManualCleanupReport,
    SessionCleanupResult, SessionCleanupStatus, DEFAULT_MANUAL_CLEANUP_MAX,
};

/// Run one manual expired-session cleanup pass. `days`/`max` are resolved
/// and validated before the runtime connects; contention on the GC lease
/// fails clearly instead of waiting, and the outcome's `failed` flag is the
/// CLI's exit-status signal. Fatal snapshot/connect errors return `Err`.
pub async fn run_manual_expired_session_cleanup(
    config: &Config,
    cluster: &str,
    options: ManualCleanupOptions,
) -> Result<ManualCleanupOutcome> {
    run_manual_cleanup_with(config, cluster, options, ManualCleanupOverrides::default()).await
}

/// An async test hook receiving the candidate's storage key.
pub type CleanupTestHook = Arc<dyn Fn(&str) -> BoxFuture<'static, ()> + Send + Sync>;

/// Deterministic test hooks for the manual pass. Production callers pass
/// [`ManualCleanupOverrides::default`]; nothing here is persisted.
#[doc(hidden)]
#[derive(Default)]
pub struct ManualCleanupOverrides {
    /// Wall clock for the retention threshold, so tests can age sessions
    /// deterministically. Production runs use the real clock. `None` means
    /// the real clock; `Some(0)` is a valid frozen clock.
    pub now_unix_secs: Option<u64>,
    /// Test hook before each candidate's read-only guards: the point to
    /// simulate a concurrent writer racing the listed snapshot.
    pub before_candidate: Option<CleanupTestHook>,
    /// Test hook after the read-only guards passed, immediately before the
    /// charged cascade entry: the point to simulate a live A2A fence
    /// refusing a race the read-only predicate already cleared.
    pub after_admission: Option<CleanupTestHook>,
    /// Test hook after a candidate is admitted and its cascade entry
    /// (revision check and A2A fence) passed, before the cascade runs: the
    /// point to simulate a concurrent writer the final canonical CAS must
    /// reject.
    pub after_entry: Option<CleanupTestHook>,
    /// Test hook that flips the GC lease to lost once this many cascades
    /// have completed, using the lease's own loss signaling.
    pub simulate_lease_loss_after: Option<usize>,
    /// Test hook: fail the pre-delete session-stream probe for exactly this
    /// storage key, injecting the broker-side stream-info failure the pass
    /// must record as a per-session error.
    pub fail_stream_probe_for: Option<String>,
}

#[doc(hidden)]
pub async fn run_manual_cleanup_with(
    config: &Config,
    cluster: &str,
    options: ManualCleanupOptions,
    overrides: ManualCleanupOverrides,
) -> Result<ManualCleanupOutcome> {
    options.validate()?;
    let lease = acquire_session_gc_lease(config, cluster)
        .await
        .context("manual expired-session cleanup: GC lease acquisition failed")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "another session cleanup pass holds the GC lease for cluster '{cluster}'; \
                 retry when it finishes"
            )
        })?;
    let pass = run_cleanup_pass(config, cluster, &options, &overrides, &lease).await;
    // Structured release on every path: completion, fatal pass error, or
    // preview. A failed release only delays the next pass until the lease
    // TTL expires, but the CLI must not report success.
    let release_failed = lease.release().await.is_err();
    match pass {
        Ok(mut report) => {
            if release_failed {
                report.errors += 1;
            }
            let failed = report.requires_failure_status();
            Ok(ManualCleanupOutcome { report, failed })
        }
        Err(error) => Err(error),
    }
}

async fn run_cleanup_pass(
    config: &Config,
    cluster: &str,
    options: &ManualCleanupOptions,
    overrides: &ManualCleanupOverrides,
    lease: &NatsSessionLease,
) -> Result<ManualCleanupReport> {
    let jetstream = config.nats_jetstream(cluster).await?;
    let metadata_kv = match jetstream.get_key_value(SESSION_METADATA_BUCKET).await {
        Ok(store) => store,
        // No metadata bucket: no metadata-backed sessions to clean. An
        // empty success, exactly like the worker pass, not an error.
        Err(error) if crate::nats_admin::kv_bucket_missing(&error) => {
            return Ok(ManualCleanupReport::empty(options));
        }
        Err(error) => return Err(error.into()),
    };
    let metadata_store = SessionMetadataStore::from_store(metadata_kv, jetstream.client().clone());
    let lease_store = load_optional_lease_store(config, cluster).await?;
    let now = overrides.now_unix_secs.unwrap_or_else(now_unix_secs);
    let threshold = cleanup_threshold(now, options.days);
    let records = metadata_store.list().await?;
    let candidates = select_candidates(&records, threshold, options.agent.as_deref());
    let mut run = CandidateRun {
        scanned: candidates.len(),
        ..CandidateRun::default()
    };
    // The processor's open handles are the loop's own locals; keeping it a
    // plain literal (no 7-argument constructor) reads as the pass state it is.
    let processor = CandidateProcessor {
        jetstream: &jetstream,
        metadata_store: &metadata_store,
        lease_store: lease_store.as_ref(),
        threshold,
        options,
        overrides,
        lease,
    };
    processor.run_pass(cluster, candidates, &mut run).await;
    Ok(ManualCleanupReport::build(options, run))
}

/// The GC lease's local loss signal and a broker-authoritative
/// revalidation, so admissions stop as soon as either disagrees.
/// `lost_watch` carries the lease's held flag (`true` while held, flipped
/// to `false` by the renewal monitor on loss).
async fn gc_lease_live(lease: &NatsSessionLease) -> bool {
    *lease.lost_watch().borrow()
        && lease.is_held()
        && lease.revalidate_ownership().await.unwrap_or_else(|error| {
            log::warn!(
                "manual expired-session cleanup: GC lease revalidation failed: err={error:#}"
            );
            false
        })
}
