//! Delete `SESSION_` streams whose session has no metadata once nothing has
//! written to them for the retention window.
//!
//! The metadata GC in [`crate::remote_session_cleanup`] finds sessions through
//! `harnx_sessions` and judges each by its activity record, which turns and
//! plan and attachment writes refresh. A stream whose metadata was never
//! written, or was lost with a recreated bucket, is invisible to it; only a
//! scan of the streams finds one. Here such a stream is idle once its last
//! write, or its creation if it holds no messages, is older than the window,
//! and an idle one is deleted unless a session lease, a scoped A2A lease or an
//! unresolved A2A task holds the session. Deletion runs the whole session
//! cascade ([`nats_admin::delete_session_state`]), so any journal rows,
//! metadata keys, attachments and plans left under the session go with it.
//!
//! Every guard is read again immediately before the delete, the stream's age
//! last. JetStream has no conditional stream delete, so a write that lands
//! between that read and the delete is lost with the stream. The window is a
//! few requests wide, and the stream had gone unwritten for the whole
//! retention period before it.

use crate::config::Config;
use crate::nats_admin;
use crate::nats_session_log::stream_name_for_session;
use crate::nats_session_metadata::{metadata_key, SessionMetadataStore, SESSION_METADATA_BUCKET};
use crate::session_stream_orphans::{is_session_stream, storage_key_from_stream, unix_secs};
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{self, kv::Operation, stream::Info as StreamInfo};
use futures_util::TryStreamExt;
use std::collections::HashSet;

/// Most streams one pass deletes. A backlog drains over several hourly passes
/// instead of as thousands of stream deletes sent to the meta leader at once;
/// the ~4,000 streams a recreated metadata bucket left behind on one cluster
/// take about two days at this rate.
pub const DEFAULT_MAX_ORPHAN_DELETIONS_PER_RUN: usize = 100;

/// Idle window and batch limit for a cleanup pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanStreamCleanupOptions {
    /// A stream is idle once its last write, or its creation if it holds no
    /// messages, is more than this many seconds old.
    pub idle_secs: u64,
    /// At most this many streams are deleted per run; the rest wait for later
    /// runs.
    pub max_deletions: usize,
}

/// Per-pass counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OrphanStreamCleanupStats {
    /// `SESSION_` streams that map to a session.
    pub streams: usize,
    /// Of those, the idle ones with no metadata: the candidates. Those the
    /// batch limit left for a later pass are counted in `deferred`, so a
    /// backlog is never under-reported.
    pub idle: usize,
    pub deleted: usize,
    /// Held by a session lease, a scoped A2A lease or an unresolved A2A task.
    pub skipped_active: usize,
    /// Gained metadata between the listing and the delete.
    pub skipped_metadata: usize,
    /// Written to between the listing and the delete.
    pub skipped_recent: usize,
    pub deferred: usize,
    /// `SESSION_` streams that can't be mapped to a session key. They are
    /// reported and never deleted.
    pub unmapped: usize,
    pub errors: usize,
}

impl OrphanStreamCleanupStats {
    /// Idle streams the pass found and deliberately kept.
    pub fn kept(&self) -> usize {
        self.skipped_active + self.skipped_metadata + self.skipped_recent + self.deferred
    }
}

/// What a guarded delete of one stream did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedOrphanDelete {
    /// The stream is gone already; nothing was done.
    AlreadyGone,
    /// The session has metadata, so the activity scan judges it instead.
    MetadataPresent,
    /// A session or A2A lease, or an unresolved A2A task, holds the session.
    Active,
    /// The stream was written to, or created, within the idle window.
    Recent,
    /// The session's state was deleted.
    Deleted,
}

/// Clock and idle window for one guarded delete.
#[derive(Debug, Clone, Copy)]
pub struct OrphanStreamCleanupWindow {
    pub now_unix_secs: u64,
    pub idle_secs: u64,
}

impl OrphanStreamCleanupWindow {
    /// Streams last written at or after this time are not idle.
    fn cutoff(&self) -> u64 {
        self.now_unix_secs.saturating_sub(self.idle_secs)
    }
}

/// Delete idle orphan streams, at most `max_deletions` of them.
///
/// `now_unix_secs` is injectable so tests can age a stream past the window
/// without waiting out the clock.
pub async fn cleanup_orphan_session_streams(
    config: &Config,
    cluster: &str,
    now_unix_secs: u64,
    options: &OrphanStreamCleanupOptions,
) -> Result<OrphanStreamCleanupStats> {
    let ctx = StreamGcContext::connect(config, cluster).await?;
    orphan_cleanup_pass(&ctx, None, now_unix_secs, options).await
}

/// Apply every guard to one session's stream, reading each from live state,
/// and delete the session's state only if they all pass.
pub async fn delete_orphan_stream_guarded(
    config: &Config,
    cluster: &str,
    storage_key: &str,
    window: OrphanStreamCleanupWindow,
) -> Result<GuardedOrphanDelete> {
    ensure!(
        window.idle_secs > 0,
        "orphan stream deletion requires a positive idle window"
    );
    let ctx = StreamGcContext::connect(config, cluster).await?;
    ctx.delete_guarded(storage_key, window).await
}

/// The handles a cleanup pass reads and deletes through.
pub(crate) struct StreamGcContext {
    pub(crate) jetstream: jetstream::Context,
    pub(crate) metadata_store: SessionMetadataStore,
    pub(crate) lease_store: Option<jetstream::kv::Store>,
}

impl StreamGcContext {
    pub(crate) async fn connect(config: &Config, cluster: &str) -> Result<Self> {
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

    /// Whether a session lease, a scoped A2A lease or an unresolved A2A task
    /// holds the session, the same checks the metadata GC makes.
    pub(crate) async fn is_active(&self, storage_key: &str) -> Result<bool> {
        if nats_admin::a2a::unresolved(&self.metadata_store, storage_key).await? {
            return Ok(true);
        }
        let leases = self.lease_store.as_ref();
        if crate::remote_session_cleanup::a2a_lease_present(leases, storage_key).await? {
            return Ok(true);
        }
        crate::remote_session_cleanup::session_lease_present(leases, storage_key).await
    }

    /// Metadata existence read from the KV stream leader. A delete or purge
    /// marker counts as absent.
    pub(crate) async fn metadata_present(&self, storage_key: &str) -> Result<bool> {
        Ok(matches!(
            self.metadata_store.leader_entry(&metadata_key(storage_key)).await?,
            Some(entry) if entry.operation == Operation::Put
        ))
    }

    async fn fresh_stream_info(&self, stream_name: &str) -> Result<Option<StreamInfo>> {
        let mut stream = match self.jetstream.get_stream(stream_name).await {
            Ok(stream) => stream,
            Err(error) if nats_admin::stream_missing(&error.kind()) => return Ok(None),
            Err(error) => {
                return Err(anyhow::Error::from(error))
                    .with_context(|| format!("failed to inspect session stream '{stream_name}'"));
            }
        };
        let info = stream
            .info()
            .await
            .with_context(|| format!("failed to inspect session stream '{stream_name}'"))?;
        Ok(Some(info.clone()))
    }

    /// Every guard, re-derived from live state, then the session cascade. The
    /// leader KV reads run first and the stream's own age last, right before
    /// the delete, which keeps the unguarded window as narrow as the API
    /// allows.
    async fn delete_guarded(
        &self,
        storage_key: &str,
        window: OrphanStreamCleanupWindow,
    ) -> Result<GuardedOrphanDelete> {
        if self.metadata_present(storage_key).await? {
            return Ok(GuardedOrphanDelete::MetadataPresent);
        }
        if self.is_active(storage_key).await? {
            return Ok(GuardedOrphanDelete::Active);
        }
        let Some(info) = self
            .fresh_stream_info(&stream_name_for_session(storage_key))
            .await?
        else {
            return Ok(GuardedOrphanDelete::AlreadyGone);
        };
        if last_write_unix_secs(&info) >= window.cutoff() {
            return Ok(GuardedOrphanDelete::Recent);
        }
        nats_admin::delete_session_state(&self.jetstream, self.lease_store.clone(), storage_key)
            .await?;
        Ok(GuardedOrphanDelete::Deleted)
    }
}

/// One idle stream found by the listing.
struct IdleStream {
    name: String,
    storage_key: String,
}

/// `known_metadata_keys` is an optional prefilter from a metadata listing the
/// caller already made (the periodic GC's activity scan): streams whose
/// session it lists are skipped without a read. It is a snapshot, and the
/// guarded delete re-reads metadata from the leader before deleting anything.
pub(crate) async fn orphan_cleanup_pass(
    ctx: &StreamGcContext,
    known_metadata_keys: Option<&HashSet<String>>,
    now_unix_secs: u64,
    options: &OrphanStreamCleanupOptions,
) -> Result<OrphanStreamCleanupStats> {
    validate_cleanup_options(options)?;
    let window = OrphanStreamCleanupWindow {
        now_unix_secs,
        idle_secs: options.idle_secs,
    };
    let mut stats = OrphanStreamCleanupStats::default();
    // List first and delete afterwards: the listing pages by offset, and
    // deleting while paging would shift later streams into pages already read.
    let mut idle = list_idle_streams(ctx, window.cutoff(), &mut stats).await?;
    if let Some(known) = known_metadata_keys {
        idle.retain(|stream| !known.contains(&stream.storage_key));
    }
    stats.idle = idle.len();
    for (index, stream) in idle.iter().enumerate() {
        if stats.deleted >= options.max_deletions {
            stats.deferred = idle.len() - index;
            log::info!(
                "orphan stream cleanup reached the batch limit of {}; {} idle orphans wait \
                 for the next pass",
                options.max_deletions,
                stats.deferred
            );
            break;
        }
        let result = ctx.delete_guarded(&stream.storage_key, window).await;
        record_delete_result(&mut stats, stream, result);
    }
    Ok(stats)
}

/// Every `SESSION_` stream last written before `cutoff`, counting the streams
/// examined and the ones that can't be mapped to a session.
async fn list_idle_streams(
    ctx: &StreamGcContext,
    cutoff: u64,
    stats: &mut OrphanStreamCleanupStats,
) -> Result<Vec<IdleStream>> {
    let mut idle = Vec::new();
    let mut streams = ctx.jetstream.streams();
    while let Some(info) = streams.try_next().await? {
        if is_session_stream(&info.config) {
            idle.extend(idle_stream(&info, cutoff, stats));
        }
    }
    Ok(idle)
}

/// `info`'s session if its stream was last written before `cutoff`. Counts
/// the stream in `stats`, as unmapped if it can't be mapped to a session.
fn idle_stream(
    info: &StreamInfo,
    cutoff: u64,
    stats: &mut OrphanStreamCleanupStats,
) -> Option<IdleStream> {
    let storage_key = match storage_key_from_stream(&info.config) {
        Ok(storage_key) => storage_key,
        Err(reason) => {
            log::warn!(
                "SESSION_ stream cannot be mapped to a session key; leaving it for manual \
                 review: stream={} subjects={:?} reason={reason}",
                info.config.name,
                info.config.subjects
            );
            stats.unmapped += 1;
            return None;
        }
    };
    stats.streams += 1;
    (last_write_unix_secs(info) < cutoff).then(|| IdleStream {
        name: info.config.name.clone(),
        storage_key,
    })
}

fn record_delete_result(
    stats: &mut OrphanStreamCleanupStats,
    stream: &IdleStream,
    result: Result<GuardedOrphanDelete>,
) {
    match result {
        Ok(GuardedOrphanDelete::Deleted) => {
            log::info!(
                "deleted idle orphan session: stream={} storage_key={}",
                stream.name,
                stream.storage_key
            );
            stats.deleted += 1;
        }
        Ok(GuardedOrphanDelete::Active) => stats.skipped_active += 1,
        Ok(GuardedOrphanDelete::Recent) => stats.skipped_recent += 1,
        Ok(GuardedOrphanDelete::MetadataPresent) => stats.skipped_metadata += 1,
        Ok(GuardedOrphanDelete::AlreadyGone) => {}
        Err(error) => {
            log::warn!(
                "orphan session delete failed: stream={} storage_key={} err={error:#}",
                stream.name,
                stream.storage_key
            );
            stats.errors += 1;
        }
    }
}

/// When the stream was last written, or created if it holds no messages.
fn last_write_unix_secs(info: &StreamInfo) -> u64 {
    if info.state.messages > 0 {
        unix_secs(info.state.last_timestamp.unix_timestamp())
    } else {
        unix_secs(info.created.unix_timestamp())
    }
}

/// A zero idle window would make every stream idle the moment it stopped
/// being written, and a zero batch would delete nothing while reporting an
/// empty backlog, so both must be positive before a pass touches anything.
pub(crate) fn validate_cleanup_options(options: &OrphanStreamCleanupOptions) -> Result<()> {
    ensure!(
        options.idle_secs > 0,
        "orphan stream cleanup requires a positive idle window (idle_secs > 0)"
    );
    ensure!(
        options.max_deletions > 0,
        "orphan stream cleanup requires a positive max_deletions"
    );
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn options(idle_secs: u64, max_deletions: usize) -> OrphanStreamCleanupOptions {
        OrphanStreamCleanupOptions {
            idle_secs,
            max_deletions,
        }
    }

    #[test]
    fn cleanup_options_reject_a_zero_idle_window() {
        let rejected = validate_cleanup_options(&options(0, 5));
        assert!(
            rejected.is_err(),
            "a zero window must never reach a deletion"
        );
        assert!(validate_cleanup_options(&options(1, 5)).is_ok());
    }

    #[test]
    fn cleanup_options_reject_a_zero_batch() {
        assert!(
            validate_cleanup_options(&options(24 * 60 * 60, 0)).is_err(),
            "zero is not a bound"
        );
    }

    #[test]
    fn the_window_cutoff_saturates() {
        let window = OrphanStreamCleanupWindow {
            now_unix_secs: 10,
            idle_secs: 60,
        };
        assert_eq!(window.cutoff(), 0);
    }
}
