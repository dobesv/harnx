//! Deletion markers that expire.
//!
//! A KV delete or purge leaves a marker on its key. With one message kept per
//! key and no `max_age` on the bucket, nothing removes that marker, so every
//! key ever deleted keeps one. A bucket that allows per-message TTLs can give
//! the marker one instead.

use async_nats::jetstream::{kv, stream};
use std::time::Duration;

/// How long the marker a deletion leaves lasts. A revision-conditioned purge
/// fences a stale writer with it, and that writer's compare-and-set still
/// fails once the marker expires, since the revision it expects no longer
/// exists.
pub const DELETION_MARKER_TTL: Duration = Duration::from_secs(60 * 60);

/// How long the marker nats-server leaves in place of an expired message
/// lasts. Readers take it for a missing key, so it need not last.
pub const EXPIRY_MARKER_TTL: Duration = Duration::from_secs(60);

/// Turn on what `kv::Config::limit_markers: Some(EXPIRY_MARKER_TTL)` turns on
/// when creating a bucket: per-message TTLs, and markers for expired messages
/// that last [`EXPIRY_MARKER_TTL`]. Pass it to
/// [`crate::registry::reconcile_bucket`] for a bucket that already exists, so
/// it ends up matching the create config and a later open doesn't reconcile
/// it again.
pub fn allow_expiring_markers(config: &mut stream::Config) {
    config.allow_message_ttl = true;
    config.subject_delete_marker_ttl = Some(EXPIRY_MARKER_TTL);
}

/// Purge `key`, at `revision` when one is given, leaving a marker that
/// expires after [`DELETION_MARKER_TTL`].
///
/// A bucket that doesn't allow per-message TTLs yet rejects a message that
/// carries one, so there the marker stays, as it always did. The store's
/// cached config decides. A stream can't turn TTLs off again, so that config
/// never claims TTLs the bucket doesn't take.
pub async fn purge(
    store: &kv::Store,
    key: &str,
    revision: Option<u64>,
) -> Result<(), kv::PurgeError> {
    if !store.stream.cached_info().config.allow_message_ttl {
        return store.purge_expect_revision(key, revision).await;
    }
    match revision {
        Some(revision) => {
            store
                .purge_expect_revision_with_ttl(key, revision, DELETION_MARKER_TTL)
                .await
        }
        None => store.purge_with_ttl(key, DELETION_MARKER_TTL).await,
    }
}
