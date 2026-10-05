//! KV reads that only the stream leader answers.
//!
//! async-nats creates KV buckets with `allow_direct`, and `kv::Store::get` and
//! `kv::Store::entry` then make direct gets, which NATS lets a follower answer
//! from whatever it has applied so far. On a replicated bucket, a read made
//! right after a write, by the writer or by whoever it handed the work to, can
//! miss that write. These reads go through `STREAM.MSG.GET` instead, which only
//! the stream leader answers, so they see every acknowledged write. The request
//! doesn't depend on the bucket's `allow_direct`, so it holds for buckets that
//! already exist with it on.
//!
//! While the stream has no leader the request is refused, so the read fails
//! rather than reporting the key missing.
use anyhow::{ensure, Context, Result};
use async_nats::jetstream::{
    kv,
    message::StreamMessage,
    stream::{LastRawMessageError, LastRawMessageErrorKind},
    ErrorCode,
};
use bytes::Bytes;
use std::future::Future;
use tokio::time::Instant;

/// The header a KV store writes on the markers that delete and purge a key.
const KV_OPERATION: &str = "KV-Operation";

/// The latest revision of `key`, which may be a delete or purge marker.
pub async fn entry(store: &kv::Store, key: &str) -> Result<Option<kv::Entry>> {
    // The leader takes this subject as a filter, so a wildcard in a key would
    // answer with some other key's revision.
    ensure!(
        is_valid_key(key),
        "invalid key {key} in bucket {}",
        store.name
    );
    entry_matching(store, key).await
}

/// The latest revision, which may be a delete or purge marker, of whichever
/// key matches `pattern`: a key some of whose dot-separated tokens are `*`,
/// each standing for any one token. The entry names the key that matched.
///
/// The leader answers with the most recently written of every matching key,
/// so callers use this only where at most one key can match.
pub async fn entry_matching(store: &kv::Store, pattern: &str) -> Result<Option<kv::Entry>> {
    // A token that only contains a `*`, or a `>`, would match keys the
    // caller never meant.
    ensure!(
        pattern
            .split('.')
            .all(|token| token == "*" || (!token.is_empty() && is_valid_key(token))),
        "invalid key pattern {pattern} in bucket {}",
        store.name
    );
    let subject = format!("{}{pattern}", store.prefix);
    let message = match store.stream.get_last_raw_message_by_subject(&subject).await {
        Ok(message) => message,
        Err(error) if matches!(error.kind(), LastRawMessageErrorKind::NoMessageFound) => {
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read key {pattern} from bucket {}", store.name));
        }
    };
    let key = message
        .subject
        .strip_prefix(store.prefix.as_str())
        .unwrap_or(pattern)
        .to_owned();
    Ok(Some(kv::Entry {
        bucket: store.name.clone(),
        key,
        operation: operation(&message),
        value: message.payload,
        revision: message.sequence,
        delta: 0,
        created: message.time,
        seen_current: false,
    }))
}

/// `key`'s value, or `None` once it has been deleted or purged, as
/// `kv::Store::get` answers.
pub async fn get(store: &kv::Store, key: &str) -> Result<Option<Bytes>> {
    Ok(entry(store, key)
        .await?
        .filter(|entry| entry.operation == kv::Operation::Put)
        .map(|entry| entry.value))
}

/// Retry a leader read while its failure is transient, for as long as
/// [`crate::recovery::read`] retries. Nothing answers a read while the stream
/// elects a leader, where a follower used to, and an election is over in
/// seconds. A stream that doesn't exist fails the same way every time.
pub async fn retry_transient<T, F, Fut>(operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let deadline = Instant::now() + crate::recovery::RECOVERY_TIMEOUT;
    crate::recovery::retry_until(deadline, operation, is_transient).await
}

/// Whether a failed read may succeed when retried: the request went
/// unanswered, or the cluster said the stream is unavailable for now.
fn is_transient(error: &anyhow::Error) -> bool {
    let Some(read) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<LastRawMessageError>())
    else {
        return false;
    };
    match read.kind() {
        LastRawMessageErrorKind::Other => true,
        LastRawMessageErrorKind::JetStream(error) => {
            [ErrorCode::CLUSTER_NOT_AVAILABLE, ErrorCode::STREAM_OFFLINE]
                .contains(&error.error_code())
        }
        LastRawMessageErrorKind::NoMessageFound | LastRawMessageErrorKind::InvalidSubject => false,
    }
}

/// What a revision did to its key, decoded as `kv::Store` decodes it. The
/// server marks a key it removed itself, for an age limit say, with a marker
/// reason rather than the KV operation header.
fn operation(message: &StreamMessage) -> kv::Operation {
    if let Some(operation) = message.headers.get(KV_OPERATION) {
        return operation.as_str().parse().unwrap_or(kv::Operation::Put);
    }
    let reason = message.headers.get(async_nats::header::NATS_MARKER_REASON);
    match reason.map(|reason| reason.as_str()) {
        Some("MaxAge" | "Purge") => kv::Operation::Purge,
        Some("Remove") => kv::Operation::Delete,
        _ => kv::Operation::Put,
    }
}

/// The keys `kv::Store` itself accepts.
fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('.')
        && !key.ends_with('.')
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-/_=.".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused_with(err_code: u64) -> anyhow::Error {
        let error: async_nats::jetstream::Error = serde_json::from_value(serde_json::json!({
            "code": 503,
            "err_code": err_code,
            "description": "refused by the test",
        }))
        .unwrap();
        anyhow::Error::new(LastRawMessageError::new(
            LastRawMessageErrorKind::JetStream(error),
        ))
        .context("read key k from bucket b")
    }

    #[test]
    fn unanswered_and_unavailable_reads_are_retried() {
        let unanswered =
            anyhow::Error::new(LastRawMessageError::new(LastRawMessageErrorKind::Other))
                .context("read key k from bucket b");
        assert!(is_transient(&unanswered));
        assert!(is_transient(&refused_with(10008)));
        assert!(is_transient(&refused_with(10118)));
    }

    #[test]
    fn a_missing_stream_or_an_invalid_key_fails_at_once() {
        assert!(!is_transient(&refused_with(10059)));
        assert!(!is_transient(&anyhow::Error::new(
            LastRawMessageError::new(LastRawMessageErrorKind::InvalidSubject)
        )));
        assert!(!is_transient(&anyhow::anyhow!(
            "invalid key k* in bucket b"
        )));
    }
}
