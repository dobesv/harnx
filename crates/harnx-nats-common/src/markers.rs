//! Deletion markers that expire.
//!
//! A KV delete or purge leaves a marker on its key. With one message kept per
//! key and no `max_age` on the bucket, nothing removes that marker, so every
//! key ever deleted keeps one. A bucket that allows per-message TTLs can give
//! the marker one instead.
//!
//! Only where nothing depends on the marker staying. A strict create,
//! `update(key, value, 0)`, fails over a marker but succeeds once it has
//! expired, so a key guarded that way against being revived after deletion,
//! as A2A records are, has to keep a permanent marker.

use async_nats::jetstream::{kv, stream};
use std::time::Duration;

/// The header a KV store writes on the markers that delete and purge a key.
const KV_OPERATION: &str = "KV-Operation";

/// How long the marker a deletion leaves lasts. A revision-conditioned purge
/// fences a stale writer with it, and that writer's compare-and-set at a
/// revision above zero still fails once the marker expires, since the
/// revision it expects no longer exists. A strict create (revision zero)
/// does not; see the module docs.
pub const DELETION_MARKER_TTL: Duration = Duration::from_secs(60 * 60);

/// What a revision did to its key, decoded by the one decoder every reader
/// shares. The store writes the `KV-Operation` header itself; the server
/// instead marks a revision it removed — a per-message TTL or age limit —
/// with a marker reason header only.
pub fn operation(headers: Option<&async_nats::HeaderMap>) -> kv::Operation {
    let Some(headers) = headers else {
        return kv::Operation::Put;
    };
    if let Some(operation) = headers.get(KV_OPERATION) {
        return operation.as_str().parse().unwrap_or(kv::Operation::Put);
    }
    let reason = headers.get(async_nats::header::NATS_MARKER_REASON);
    match reason.map(|reason| reason.as_str()) {
        Some("MaxAge" | "Purge") => kv::Operation::Purge,
        Some("Remove") => kv::Operation::Delete,
        _ => kv::Operation::Put,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> async_nats::HeaderMap {
        let mut headers = async_nats::HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, *value);
        }
        headers
    }

    #[test]
    fn store_operation_header_decodes() {
        assert_eq!(
            operation(Some(&headers(&[("KV-Operation", "PUT")]))),
            kv::Operation::Put
        );
        assert_eq!(
            operation(Some(&headers(&[("KV-Operation", "DEL")]))),
            kv::Operation::Delete
        );
        assert_eq!(
            operation(Some(&headers(&[("KV-Operation", "PURGE")]))),
            kv::Operation::Purge
        );
    }

    #[test]
    fn unknown_store_operation_falls_back_to_put() {
        // kv::Store writes only the three above; anything else is a newer
        // server's phrasing, and a Put is the only safe default: the reader
        // still sees the revision rather than resurrecting or hiding a key.
        assert_eq!(
            operation(Some(&headers(&[("KV-Operation", "???")]))),
            kv::Operation::Put
        );
    }

    #[test]
    fn server_expiry_markers_decode_from_marker_reason() {
        // What nats-server writes when it removes a message itself: no
        // KV-Operation header, only the marker reason. MaxAge is the TTL and
        // stream-max-age case; Remove is what a subject-limit or
        // discard-new-per-subject sweep leaves.
        assert_eq!(
            operation(Some(&headers(&[("Nats-Marker-Reason", "MaxAge")]))),
            kv::Operation::Purge
        );
        assert_eq!(
            operation(Some(&headers(&[("Nats-Marker-Reason", "Purge")]))),
            kv::Operation::Purge
        );
        assert_eq!(
            operation(Some(&headers(&[("Nats-Marker-Reason", "Remove")]))),
            kv::Operation::Delete
        );
    }

    #[test]
    fn no_headers_decode_as_put() {
        assert_eq!(operation(None), kv::Operation::Put);
        assert_eq!(operation(Some(&headers(&[]))), kv::Operation::Put);
    }

    #[test]
    fn unknown_marker_reason_falls_back_to_put() {
        // Both headers present is a server phrasing no harnx bucket sees
        // today; the store's own operation header stays authoritative.
        assert_eq!(
            operation(Some(&headers(&[
                ("KV-Operation", "DEL"),
                ("Nats-Marker-Reason", "MaxAge")
            ]))),
            kv::Operation::Delete
        );
        // An unrecognized reason keeps the old behavior rather than guessing:
        // the revision still counts as a Put.
        assert_eq!(
            operation(Some(&headers(&[("Nats-Marker-Reason", "Future")]))),
            kv::Operation::Put
        );
    }
}
