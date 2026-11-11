//! Delete `SESSION_` streams nothing has written to for the retention window,
//! whether or not their session still has metadata.
//!
//! The metadata GC in [`crate::remote_session_cleanup`] finds sessions through
//! `harnx_sessions` and judges each by its activity record. That misses a
//! stream whose metadata is gone, which only a scan of the streams can find,
//! and a stream whose activity record readers keep renewing (attachment and
//! plan reads touch it) although nothing writes to the session. Here a stream
//! is idle once its last write, or its creation if it holds no messages, is
//! older than the window, and an idle stream is deleted unless a session
//! lease, a scoped A2A lease or an unresolved A2A task holds it. Deletion runs
//! the whole session cascade ([`nats_admin::delete_session_state`]), so the
//! session's journal rows, metadata keys, attachments and plans go with it.
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

/// Most streams one pass deletes. A backlog drains over several hourly passes
/// instead of as thousands of stream deletes sent to the meta leader at once;
/// the ~4,000 streams a recreated metadata bucket left behind on one cluster
/// take about two days at this rate.
pub const DEFAULT_MAX_STREAM_DELETIONS_PER_RUN: usize = 100;

/// Which idle streams a pass may delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamScope {
    /// Only streams whose session has no metadata (`harnx delete orphan-streams`).
    Orphans,
    /// Every idle session stream (the worker's hourly GC).
    All,
}

/// Idle window, batch limit and scope for a cleanup pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCleanupOptions {
    /// A stream is idle once its last write, or its creation if it holds no
    /// messages, is more than this many seconds old.
    pub idle_secs: u64,
    /// At most this many streams are deleted per run; the rest wait for later
    /// runs.
    pub max_deletions: usize,
    pub scope: StreamScope,
}

/// Per-pass counters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StreamCleanupStats {
    /// `SESSION_` streams that map to a session.
    pub streams: usize,
    /// Of those, the idle ones: the candidates. Those the batch limit left for
    /// a later pass are counted in `deferred`, so a backlog is never
    /// under-reported.
    pub idle: usize,
    pub deleted: usize,
    /// Held by a session lease, a scoped A2A lease or an unresolved A2A task.
    pub skipped_active: usize,
    /// The session has metadata, and the pass deletes orphans only.
    pub skipped_metadata: usize,
    /// Written to between the listing and the delete.
    pub skipped_recent: usize,
    pub deferred: usize,
    /// `SESSION_` streams that can't be mapped to a session key. They are
    /// reported and never deleted.
    pub unmapped: usize,
    pub errors: usize,
}

impl StreamCleanupStats {
    /// Idle streams the pass found and deliberately kept.
    pub fn kept(&self) -> usize {
        self.skipped_active + self.skipped_metadata + self.skipped_recent + self.deferred
    }
}

/// What a guarded delete of one stream did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedStreamDelete {
    /// The stream is gone already; nothing was done.
    AlreadyGone,
    /// The session has metadata, and the scope is orphans only.
    MetadataPresent,
    /// A session or A2A lease, or an unresolved A2A task, holds the session.
    Active,
    /// The stream was written to, or created, within the idle window.
    Recent,
    /// The session's state was deleted. `orphan` says it had no metadata.
    Deleted { orphan: bool },
}

/// Clock, idle window and scope for one guarded delete.
#[derive(Debug, Clone, Copy)]
pub struct StreamCleanupWindow {
    pub now_unix_secs: u64,
    pub idle_secs: u64,
    pub scope: StreamScope,
}

impl StreamCleanupWindow {
    /// Streams last written at or after this time are not idle.
    fn cutoff(&self) -> u64 {
        self.now_unix_secs.saturating_sub(self.idle_secs)
    }
}

/// Delete the idle streams `options` selects, at most `max_deletions` of them.
///
/// `now_unix_secs` is injectable so tests can age a stream past the window
/// without waiting out the clock.
pub async fn cleanup_session_streams(
    config: &Config,
    cluster: &str,
    now_unix_secs: u64,
    options: &StreamCleanupOptions,
) -> Result<StreamCleanupStats> {
    let ctx = StreamGcContext::connect(config, cluster).await?;
    stream_cleanup_pass(&ctx, now_unix_secs, options).await
}

/// Apply every guard to one session's stream, reading each from live state,
/// and delete the session's state only if they all pass.
pub async fn delete_idle_stream_guarded(
    config: &Config,
    cluster: &str,
    storage_key: &str,
    window: StreamCleanupWindow,
) -> Result<GuardedStreamDelete> {
    ensure!(
        window.idle_secs > 0,
        "idle stream deletion requires a positive idle window"
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
        window: StreamCleanupWindow,
    ) -> Result<GuardedStreamDelete> {
        let has_metadata = self.metadata_present(storage_key).await?;
        if has_metadata && window.scope == StreamScope::Orphans {
            return Ok(GuardedStreamDelete::MetadataPresent);
        }
        if self.is_active(storage_key).await? {
            return Ok(GuardedStreamDelete::Active);
        }
        let Some(info) = self
            .fresh_stream_info(&stream_name_for_session(storage_key))
            .await?
        else {
            return Ok(GuardedStreamDelete::AlreadyGone);
        };
        if last_write_unix_secs(&info) >= window.cutoff() {
            return Ok(GuardedStreamDelete::Recent);
        }
        nats_admin::delete_session_state(&self.jetstream, self.lease_store.clone(), storage_key)
            .await?;
        Ok(GuardedStreamDelete::Deleted {
            orphan: !has_metadata,
        })
    }
}

/// One idle stream found by the listing.
struct IdleStream {
    name: String,
    storage_key: String,
}

pub(crate) async fn stream_cleanup_pass(
    ctx: &StreamGcContext,
    now_unix_secs: u64,
    options: &StreamCleanupOptions,
) -> Result<StreamCleanupStats> {
    validate_cleanup_options(options)?;
    let window = StreamCleanupWindow {
        now_unix_secs,
        idle_secs: options.idle_secs,
        scope: options.scope,
    };
    let mut stats = StreamCleanupStats::default();
    // List first and delete afterwards: the listing pages by offset, and
    // deleting while paging would shift later streams into pages already read.
    let idle = list_idle_streams(ctx, window.cutoff(), &mut stats).await?;
    stats.idle = idle.len();
    for (index, stream) in idle.iter().enumerate() {
        if stats.deleted >= options.max_deletions {
            stats.deferred = idle.len() - index;
            log::info!(
                "session stream cleanup reached the batch limit of {}; {} idle streams wait \
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
    stats: &mut StreamCleanupStats,
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
    stats: &mut StreamCleanupStats,
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
    stats: &mut StreamCleanupStats,
    stream: &IdleStream,
    result: Result<GuardedStreamDelete>,
) {
    match result {
        Ok(GuardedStreamDelete::Deleted { orphan }) => {
            log::info!(
                "deleted idle session: stream={} storage_key={} metadata={}",
                stream.name,
                stream.storage_key,
                if orphan { "absent" } else { "present" }
            );
            stats.deleted += 1;
        }
        Ok(GuardedStreamDelete::Active) => stats.skipped_active += 1,
        Ok(GuardedStreamDelete::Recent) => stats.skipped_recent += 1,
        Ok(GuardedStreamDelete::MetadataPresent) => stats.skipped_metadata += 1,
        Ok(GuardedStreamDelete::AlreadyGone) => {}
        Err(error) => {
            log::warn!(
                "idle session delete failed: stream={} storage_key={} err={error:#}",
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
pub(crate) fn validate_cleanup_options(options: &StreamCleanupOptions) -> Result<()> {
    ensure!(
        options.idle_secs > 0,
        "session stream cleanup requires a positive idle window (idle_secs > 0)"
    );
    ensure!(
        options.max_deletions > 0,
        "session stream cleanup requires a positive max_deletions"
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

    fn options(idle_secs: u64, max_deletions: usize) -> StreamCleanupOptions {
        StreamCleanupOptions {
            idle_secs,
            max_deletions,
            scope: StreamScope::All,
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
        let window = StreamCleanupWindow {
            now_unix_secs: 10,
            idle_secs: 60,
            scope: StreamScope::All,
        };
        assert_eq!(window.cutoff(), 0);
    }
}
