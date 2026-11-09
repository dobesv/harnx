//! Reconcile `SESSION_` JetStream streams that have no session metadata.
//!
//! Older session-creation paths created the stream (`nats_session_log::ensure_stream`)
//! before writing canonical metadata (`sessions/<storage_key>/meta` in
//! `harnx_sessions`). Failed metadata writes left empty `SESSION_` streams behind.
//! Creation is now metadata-first, but the metadata GC cannot discover those
//! legacy streams by enumerating KV metadata alone (GH #2650).
//!
//! The stream name is `SESSION_<sha256(storage_key)>` and cannot be reversed,
//! so streams are mapped back to their storage key through the stream subject
//! (`sessions.<storage_key>.log`) and verified against
//! [`crate::nats_session_log::stream_name_for_session`]. Streams that cannot be
//! mapped are reported and never touched.
//!
//! Deletion is deliberately conservative: only streams that are older than a
//! grace period, still hold zero messages, have no session or A2A lease, no
//! unresolved A2A tasks, and still have no metadata when re-checked from the
//! leader immediately before the delete are collected. Non-empty orphans are
//! quarantined — reported and logged for manual review, never auto-deleted.

use crate::config::Config;
use crate::nats_admin;
use crate::nats_session_log::{stream_name_for_session, STREAM_NAME_PREFIX};
use crate::nats_session_metadata::{metadata_key, SessionMetadataStore, SESSION_METADATA_BUCKET};
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, kv::Operation, stream::Info as StreamInfo};
use futures_util::TryStreamExt;

/// Default grace period, in seconds, for [`OrphanStreamCleanupOptions`]'s
/// `Default` impl. Production callers pass an explicit grace (the periodic GC
/// reuses the retention window; the CLI takes `--grace-hours` or the configured
/// retention).
const DEFAULT_GRACE_SECS: u64 = 24 * 60 * 60;

/// The periodic GC deletes at most this many orphan streams per pass. Orphans
/// are rare, so a small bound spreads the risk of a bad inventory across runs.
pub const DEFAULT_MAX_ORPHAN_DELETIONS_PER_RUN: usize = 5;

/// One `SESSION_` stream with no session metadata behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanStreamReport {
    pub stream_name: String,
    /// Storage key recovered from the stream subject and verified against the
    /// stream name.
    pub storage_key: String,
    pub created_unix_secs: u64,
    pub messages: u64,
    pub bytes: u64,
    pub last_message_unix_secs: Option<u64>,
    pub session_lease_present: bool,
    pub a2a_lease_present: bool,
    pub a2a_unresolved_tasks: bool,
}

/// A `SESSION_` stream whose subject could not be mapped to a storage key.
/// Reported for manual review; never deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmappedStreamReport {
    pub stream_name: String,
    pub subjects: Vec<String>,
    pub reason: String,
}

/// All `SESSION_` streams on one cluster, split into mapped orphans (no
/// metadata) and unmappable streams.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OrphanStreamInventory {
    pub orphans: Vec<OrphanStreamReport>,
    pub unmapped: Vec<UnmappedStreamReport>,
}

/// Grace and batch controls for orphan-stream cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanStreamCleanupOptions {
    /// A stream must have been created more than this many seconds ago.
    pub grace_secs: u64,
    /// At most this many streams are deleted per run; the rest stay for later
    /// runs.
    pub max_deletions: usize,
}

impl Default for OrphanStreamCleanupOptions {
    fn default() -> Self {
        Self {
            grace_secs: DEFAULT_GRACE_SECS,
            max_deletions: DEFAULT_MAX_ORPHAN_DELETIONS_PER_RUN,
        }
    }
}

/// Per-pass counters for orphan-stream cleanup. `skipped_metadata` counts
/// streams whose metadata appeared between the inventory and the immediate
/// pre-delete recheck. `deferred` counts candidates the pass never examined
/// because the deletion batch filled, so `scanned` never under-reports the
/// backlog (GH #2650 review F7).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OrphanStreamCleanupStats {
    pub scanned: usize,
    pub deleted: usize,
    pub quarantined: usize,
    pub skipped_recent: usize,
    pub skipped_active: usize,
    pub skipped_metadata: usize,
    pub deferred: usize,
    pub unmapped: usize,
    pub errors: usize,
}

impl OrphanStreamCleanupStats {
    /// Number of streams the pass found but deliberately did not delete.
    pub fn retained(&self) -> usize {
        self.quarantined
            + self.skipped_recent
            + self.skipped_active
            + self.skipped_metadata
            + self.deferred
    }
}

/// Result of the single-stream guarded delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedOrphanDelete {
    /// The stream is gone already; nothing was done.
    AlreadyGone,
    /// Metadata exists; not an orphan (or it stopped being one).
    MetadataPresent,
    /// Session or A2A lease, or unresolved A2A tasks, hold the stream.
    Active,
    /// The stream holds messages; quarantined for manual review.
    NonEmpty { messages: u64 },
    /// The stream is younger than the grace period; kept for a later pass.
    TooRecent,
    /// All guards passed and the stream was deleted by name.
    Deleted,
}

/// List all `SESSION_` streams on `cluster` that have no session metadata.
///
/// Read-only; used by `harnx list orphan-streams`. Metadata is read from the
/// stream leader like every other metadata read, so a follower that has not
/// applied a just-written metadata record cannot produce a false orphan. The
/// immediate pre-delete recheck in the cleanup pass covers the remaining race.
pub async fn list_orphan_session_streams(
    config: &Config,
    cluster: &str,
) -> Result<OrphanStreamInventory> {
    let ctx = OrphanCleanupContext::connect(config, cluster).await?;
    ctx.inventory(None).await
}

/// Reconcile orphan streams: inventory, then delete those that pass every
/// guard in [`GuardedOrphanDelete`].
///
/// `now_unix_secs` is injectable so tests can place a stream beyond the grace
/// period without waiting out the clock.
pub async fn cleanup_orphan_session_streams(
    config: &Config,
    cluster: &str,
    now_unix_secs: u64,
    options: &OrphanStreamCleanupOptions,
) -> Result<OrphanStreamCleanupStats> {
    let ctx = OrphanCleanupContext::connect(config, cluster).await?;
    orphan_cleanup_pass(&ctx, None, now_unix_secs, options).await
}

/// Clock and grace window for a single-stream guarded delete.
#[derive(Debug, Clone, Copy)]
pub struct OrphanStreamCleanupWindow {
    pub now_unix_secs: u64,
    pub grace_secs: u64,
}

/// Apply every deletion guard to one orphan stream and delete it only by
/// name if they all pass. All guards are re-derived from live state: a
/// leader metadata read, lease/A2A checks, and a fresh stream info read
/// (emptiness plus the grace window) immediately before the delete, so
/// state that appeared after an earlier inventory still protects the
/// stream.
pub async fn delete_orphan_stream_guarded(
    config: &Config,
    cluster: &str,
    storage_key: &str,
    window: OrphanStreamCleanupWindow,
) -> Result<GuardedOrphanDelete> {
    let ctx = OrphanCleanupContext::connect(config, cluster).await?;
    ctx.delete_guarded(storage_key, window.now_unix_secs, window.grace_secs)
        .await
}

pub(crate) struct OrphanCleanupContext {
    jetstream: jetstream::Context,
    metadata_store: SessionMetadataStore,
    lease_store: Option<jetstream::kv::Store>,
}

impl OrphanCleanupContext {
    async fn connect(config: &Config, cluster: &str) -> Result<Self> {
        let jetstream = config.nats_jetstream(cluster).await?;
        let metadata_store = connect_metadata_store(&jetstream).await?;
        let lease_store = connect_lease_store(config, cluster).await?;
        Ok(Self::from_parts(jetstream, metadata_store, lease_store))
    }

    /// From existing handles, so the periodic GC pass does not reconnect.
    pub(crate) fn from_parts(
        jetstream: jetstream::Context,
        metadata_store: SessionMetadataStore,
        lease_store: Option<jetstream::kv::Store>,
    ) -> Self {
        Self {
            jetstream,
            metadata_store,
            lease_store,
        }
    }

    /// List `SESSION_` streams with no session metadata.
    ///
    /// `known_metadata_keys` is an optional prefilter from a listing the
    /// caller already made (the periodic GC's metadata pass). It is a
    /// snapshot, not a live read: a stream whose metadata landed after that
    /// listing still gets the leader read below, and a stale entry only
    /// defers reporting to the next pass — deletion always re-verifies with
    /// [`Self::delete_guarded`].
    async fn inventory(
        &self,
        known_metadata_keys: Option<&std::collections::HashSet<String>>,
    ) -> Result<OrphanStreamInventory> {
        let mut inventory = OrphanStreamInventory::default();
        let mut streams = self.jetstream.streams();
        while let Some(info) = streams.try_next().await? {
            if !is_session_stream_name(&info.config.name) {
                continue;
            }
            self.inventory_stream(&mut inventory, &info, known_metadata_keys)
                .await?;
        }
        Ok(inventory)
    }

    async fn inventory_stream(
        &self,
        inventory: &mut OrphanStreamInventory,
        info: &StreamInfo,
        known_metadata_keys: Option<&std::collections::HashSet<String>>,
    ) -> Result<()> {
        match storage_key_from_stream(&info.config.name, &info.config.subjects) {
            Ok(storage_key) => {
                if known_metadata_keys.is_some_and(|known| known.contains(&storage_key)) {
                    return Ok(());
                }
                if session_metadata_present(&self.metadata_store, &storage_key).await? {
                    return Ok(());
                }
                let report = self.report_orphan(info, &storage_key).await?;
                inventory.orphans.push(report);
            }
            Err(reason) => {
                log::warn!(
                    "SESSION_ stream cannot be mapped to a session key; leaving it for \
                     manual review: cluster stream={} subjects={:?} reason={reason}",
                    info.config.name,
                    info.config.subjects
                );
                inventory.unmapped.push(UnmappedStreamReport {
                    stream_name: info.config.name.clone(),
                    subjects: info.config.subjects.clone(),
                    reason,
                });
            }
        }
        Ok(())
    }

    async fn report_orphan(
        &self,
        info: &StreamInfo,
        storage_key: &str,
    ) -> Result<OrphanStreamReport> {
        let session_lease_present = crate::remote_session_cleanup::session_lease_present(
            self.lease_store.as_ref(),
            storage_key,
        )
        .await?;
        let a2a_lease_present = crate::remote_session_cleanup::a2a_lease_present(
            self.lease_store.as_ref(),
            storage_key,
        )
        .await?;
        let a2a_unresolved_tasks =
            nats_admin::a2a::unresolved(&self.metadata_store, storage_key).await?;
        Ok(OrphanStreamReport {
            stream_name: info.config.name.clone(),
            storage_key: storage_key.to_string(),
            created_unix_secs: unix_secs(info.created.unix_timestamp()),
            messages: info.state.messages,
            bytes: info.state.bytes,
            last_message_unix_secs: (info.state.messages > 0)
                .then(|| unix_secs(info.state.last_timestamp.unix_timestamp())),
            session_lease_present,
            a2a_lease_present,
            a2a_unresolved_tasks,
        })
    }

    async fn fresh_stream_info(&self, stream_name: &str) -> Result<Option<StreamInfo>> {
        let mut stream = match self.jetstream.get_stream(&stream_name).await {
            Ok(stream) => stream,
            Err(error) if nats_admin::stream_missing(&error.kind()) => return Ok(None),
            Err(error) => {
                return Err(anyhow::Error::from(error))
                    .with_context(|| format!("failed to inspect orphan stream '{stream_name}'"));
            }
        };
        let info = stream
            .info()
            .await
            .with_context(|| format!("failed to inspect orphan stream '{stream_name}'"))?;
        Ok(Some(info.clone()))
    }

    /// Every deletion guard, re-derived from live state, then delete by name.
    ///
    /// Guard order is deliberate (GH #2650 review F3): the leader KV reads
    /// (metadata, leases, A2A) run first, and the stream re-read — the one
    /// guard JetStream offers no conditional delete for — runs last,
    /// immediately before `delete_stream`, so the residual TOCTOU window is
    /// as narrow as the API allows.
    async fn delete_guarded(
        &self,
        storage_key: &str,
        now_unix_secs: u64,
        grace_secs: u64,
    ) -> Result<GuardedOrphanDelete> {
        // Defensive: a zero grace period would make even a stream created a
        // second ago a deletion candidate. Callers validate their options
        // before the pass; this is the last line before any deletion.
        ensure!(
            grace_secs > 0,
            "orphan stream deletion requires a positive grace period"
        );
        // Metadata first: a stream with canonical metadata is not an orphan
        // at all, whatever its stream state says.
        if session_metadata_present(&self.metadata_store, storage_key).await? {
            return Ok(GuardedOrphanDelete::MetadataPresent);
        }
        if orphan_stream_is_active(self, storage_key).await? {
            return Ok(GuardedOrphanDelete::Active);
        }
        // Re-read the stream instead of trusting inventory data: a message
        // published in between would turn this into a destructive delete.
        let stream_name = stream_name_for_session(storage_key);
        let Some(info) = self.fresh_stream_info(&stream_name).await? else {
            return Ok(GuardedOrphanDelete::AlreadyGone);
        };
        if info.state.messages > 0 {
            return Ok(GuardedOrphanDelete::NonEmpty {
                messages: info.state.messages,
            });
        }
        // The grace window is re-checked here, not just against the
        // inventory's snapshot: a stream recreated after the inventory is
        // young again and must not be deleted this pass.
        if unix_secs(info.created.unix_timestamp()) >= now_unix_secs.saturating_sub(grace_secs) {
            return Ok(GuardedOrphanDelete::TooRecent);
        }
        // Residual window: between this info() and delete_stream a message or
        // metadata write can still land — JetStream has no conditional stream
        // delete, so the race can only be shrunk, never closed. It is
        // acceptable because session creation is metadata-first (the
        // transcript's publish path writes canonical metadata before or with
        // the first message), every guard above was re-derived from live
        // state moments ago, the stream must already have survived the full
        // grace period, and deletions are capped per pass, so a stream that
        // loses this race is empty and metadata-less by every observable
        // measure.
        nats_admin::delete_session_stream(&self.jetstream, storage_key)
            .await
            .map(|deleted| {
                if deleted {
                    GuardedOrphanDelete::Deleted
                } else {
                    GuardedOrphanDelete::AlreadyGone
                }
            })
    }
}

async fn connect_metadata_store(jetstream: &jetstream::Context) -> Result<SessionMetadataStore> {
    let metadata_kv = match jetstream.get_key_value(SESSION_METADATA_BUCKET).await {
        Ok(store) => store,
        Err(error) if nats_admin::kv_bucket_missing(&error) => {
            return Err(error).with_context(|| {
                format!("session metadata bucket '{SESSION_METADATA_BUCKET}' not found")
            });
        }
        Err(error) => return Err(error.into()),
    };
    Ok(SessionMetadataStore::from_store(
        metadata_kv,
        jetstream.client().clone(),
    ))
}

async fn connect_lease_store(
    config: &Config,
    cluster: &str,
) -> Result<Option<jetstream::kv::Store>> {
    match config.nats_kv_bucket(cluster, "harnx_leases").await {
        Ok(store) => Ok(Some(store)),
        Err(error) if lease_bucket_missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn lease_bucket_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<jetstream::context::KeyValueError>())
        .is_some_and(nats_admin::kv_bucket_missing)
}

/// The lease/A2A half of the metadata GC's `candidate_is_active` checks: no
/// session lease, no scoped A2A lease, no unresolved A2A tasks.
async fn orphan_stream_is_active(ctx: &OrphanCleanupContext, storage_key: &str) -> Result<bool> {
    if nats_admin::a2a::unresolved(&ctx.metadata_store, storage_key).await? {
        return Ok(true);
    }
    if crate::remote_session_cleanup::a2a_lease_present(ctx.lease_store.as_ref(), storage_key)
        .await?
    {
        return Ok(true);
    }
    if crate::remote_session_cleanup::session_lease_present(ctx.lease_store.as_ref(), storage_key)
        .await?
    {
        return Ok(true);
    }
    Ok(false)
}

pub(crate) async fn orphan_cleanup_pass(
    ctx: &OrphanCleanupContext,
    known_metadata_keys: Option<&std::collections::HashSet<String>>,
    now_unix_secs: u64,
    options: &OrphanStreamCleanupOptions,
) -> Result<OrphanStreamCleanupStats> {
    validate_cleanup_options(options)?;
    let mut stats = OrphanStreamCleanupStats::default();
    let inventory = ctx.inventory(known_metadata_keys).await?;
    // Count every candidate up front so the batch cap can't make the backlog
    // look smaller than it is: "5 found, 5 deleted" is a lie when 15 remain.
    stats.scanned = inventory.orphans.len();
    stats.unmapped = inventory.unmapped.len();
    let grace_cutoff = now_unix_secs.saturating_sub(options.grace_secs);

    for (index, orphan) in inventory.orphans.iter().enumerate() {
        if stats.deleted >= options.max_deletions {
            stats.deferred = inventory.orphans.len() - index;
            log::info!(
                "orphan session stream cleanup reached the batch limit of {}; {} more \
                 candidates are deferred to the next pass",
                options.max_deletions,
                stats.deferred
            );
            break;
        }
        if orphan.created_unix_secs >= grace_cutoff {
            stats.skipped_recent += 1;
            continue;
        }
        let result = ctx
            .delete_guarded(&orphan.storage_key, now_unix_secs, options.grace_secs)
            .await;
        record_delete_result(&mut stats, orphan, result);
    }
    Ok(stats)
}

fn record_delete_result(
    stats: &mut OrphanStreamCleanupStats,
    orphan: &OrphanStreamReport,
    result: Result<GuardedOrphanDelete>,
) {
    match result {
        Ok(GuardedOrphanDelete::Deleted) => {
            log::info!(
                "deleted orphan session stream: name={} storage_key={} created_unix_secs={}",
                orphan.stream_name,
                orphan.storage_key,
                orphan.created_unix_secs
            );
            stats.deleted += 1;
        }
        Ok(GuardedOrphanDelete::NonEmpty { messages }) => {
            log::warn!(
                "orphan session stream holds {} messages and is quarantined for manual \
                 review (never auto-deleted): name={} storage_key={}",
                messages,
                orphan.stream_name,
                orphan.storage_key
            );
            stats.quarantined += 1;
        }
        Ok(GuardedOrphanDelete::Active) => {
            stats.skipped_active += 1;
        }
        Ok(GuardedOrphanDelete::TooRecent) => {
            stats.skipped_recent += 1;
        }
        Ok(GuardedOrphanDelete::MetadataPresent) => {
            log::info!(
                "orphan session stream gained metadata before deletion; keeping it: name={} \
                 storage_key={}",
                orphan.stream_name,
                orphan.storage_key
            );
            stats.skipped_metadata += 1;
        }
        Ok(GuardedOrphanDelete::AlreadyGone) => {}
        Err(error) => {
            log::warn!(
                "orphan session stream delete failed: name={} storage_key={} err={error:#}",
                orphan.stream_name,
                orphan.storage_key
            );
            stats.errors += 1;
        }
    }
}

/// Zero would mean "delete every empty orphan immediately" for the grace
/// period and "delete nothing but report an empty backlog" for the batch, so
/// both options must be positive before the pass touches anything. Pure so
/// the bounds can be unit-tested without a NATS server.
pub(crate) fn validate_cleanup_options(options: &OrphanStreamCleanupOptions) -> Result<()> {
    ensure!(
        options.grace_secs > 0,
        "orphan stream cleanup requires a positive grace period \
         (grace_secs > 0); use a real grace window, not zero"
    );
    ensure!(
        options.max_deletions > 0,
        "orphan stream cleanup requires a positive max_deletions"
    );
    Ok(())
}

/// Metadata existence read from the KV stream leader, matching the reads the
/// session GC already does. A delete or purge marker counts as absent.
async fn session_metadata_present(store: &SessionMetadataStore, storage_key: &str) -> Result<bool> {
    Ok(matches!(
        store.leader_entry(&metadata_key(storage_key)).await?,
        Some(entry) if entry.operation == Operation::Put
    ))
}

fn is_session_stream_name(name: &str) -> bool {
    name.starts_with(STREAM_NAME_PREFIX)
}

/// Map a `SESSION_` stream back to its storage key through the stream subject
/// (`sessions.<storage_key>.log`, or the wider `sessions.<storage_key>.>`
/// some deployments create), then verify the stream name is the key's hash.
/// `Err` carries the reason the stream cannot be mapped; unmappable streams
/// are reported and never deleted.
fn storage_key_from_stream(name: &str, subjects: &[String]) -> std::result::Result<String, String> {
    for subject in subjects {
        let Some(key) = storage_key_from_subject(subject) else {
            continue;
        };
        if stream_name_for_session(&key) == name {
            return Ok(key);
        }
        return Err(format!(
            "subject '{subject}' maps to key '{key}' but the stream name is not its hash"
        ));
    }
    Err(match subjects {
        [] => "the stream has no subjects".to_string(),
        subjects => format!(
            "no subject of the form sessions.<storage_key>.log or sessions.<storage_key>.> found \
             (subjects: {subjects:?})"
        ),
    })
}

/// `sessions.<storage_key>.log` / `sessions.<storage_key>.>` ->
/// `<storage_key>`. Keys are SHA-256 hex, so a nested dot means the subject
/// belongs to something else.
fn storage_key_from_subject(subject: &str) -> Option<String> {
    let key = subject.strip_prefix("sessions.")?;
    let key = key
        .strip_suffix(".log")
        .or_else(|| key.strip_suffix(".>"))
        .or_else(|| key.strip_suffix(".*"))?;
    if key.is_empty() || key.contains('.') {
        return None;
    }
    Some(key.to_string())
}

fn unix_secs(timestamp: i64) -> u64 {
    timestamp.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_stream_prefix_matches_stream_name_for_session() {
        let name = stream_name_for_session("some-storage-key");
        assert!(is_session_stream_name(&name));
        assert!(name.starts_with(STREAM_NAME_PREFIX));
        assert!(!is_session_stream_name("KV_harnx_sessions"));
    }

    #[test]
    fn storage_key_round_trips_through_subject() {
        let key = harnx_core::session_identity::session_key(Some("athena"), "orphan-map");
        let subject = crate::nats_session_log::subject_for_session(&key);
        assert_eq!(
            storage_key_from_subject(&subject).as_deref(),
            Some(key.as_str())
        );
        assert_eq!(
            storage_key_from_subject(&format!("sessions.{key}.>")).as_deref(),
            Some(key.as_str())
        );
    }

    #[test]
    fn storage_key_rejects_unmappable_subjects() {
        assert_eq!(storage_key_from_subject("sessions..log"), None);
        assert_eq!(storage_key_from_subject("sessions.a.b.log"), None);
        assert_eq!(storage_key_from_subject("sessions.abc"), None);
        assert_eq!(storage_key_from_subject("other.subject.log"), None);
        assert_eq!(storage_key_from_subject("sessions."), None);
    }

    #[test]
    fn storage_key_from_stream_requires_name_hash_match() {
        let key = harnx_core::session_identity::session_key(Some("athena"), "hash-match");
        let subject = crate::nats_session_log::subject_for_session(&key);
        let matched = storage_key_from_stream(&stream_name_for_session(&key), &[subject]);
        assert_eq!(matched.expect("mapped"), key);

        // A stream whose subject parses but whose name is not the key's hash
        // must be reported, not mapped.
        let mismatched = storage_key_from_stream(
            &stream_name_for_session(&format!("{key}-other")),
            &[crate::nats_session_log::subject_for_session(&key)],
        );
        assert!(mismatched.is_err());

        assert!(storage_key_from_stream(&stream_name_for_session(&key), &[]).is_err());
        assert!(
            storage_key_from_stream(&stream_name_for_session(&key), &["other.subject".into()])
                .is_err()
        );
    }

    #[test]
    fn cleanup_options_default_to_one_day_grace_and_small_batches() {
        let options = OrphanStreamCleanupOptions::default();
        assert_eq!(options.grace_secs, 24 * 60 * 60);
        assert_eq!(options.max_deletions, DEFAULT_MAX_ORPHAN_DELETIONS_PER_RUN);
        assert!(options.max_deletions <= 10);
    }

    #[test]
    fn cleanup_options_reject_zero_grace() {
        let rejected = validate_cleanup_options(&OrphanStreamCleanupOptions {
            grace_secs: 0,
            max_deletions: 5,
        });
        assert!(rejected.is_err(), "zero grace must never reach a deletion");
        assert!(validate_cleanup_options(&OrphanStreamCleanupOptions {
            grace_secs: 1,
            max_deletions: 5,
        })
        .is_ok());
    }

    #[test]
    fn cleanup_options_reject_zero_batch() {
        let rejected = validate_cleanup_options(&OrphanStreamCleanupOptions {
            grace_secs: 24 * 60 * 60,
            max_deletions: 0,
        });
        assert!(rejected.is_err(), "zero batch is not a valid bound");
    }
}
