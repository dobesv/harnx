//! Registry-watch freshness for NATS tool discovery (decision 5b).
//!
//! The worker's 30-second discovery cache must not delay a newly written or
//! deleted registration to the next completion. Two mechanisms cooperate:
//!
//! - [`registry_state_revision`] is the deterministic guarantee. The registry
//!   bucket's stream sequence moves synchronously with every write and
//!   delete, so the next completion compares it against the sequence a
//!   cached snapshot started scanning under. No event delivery is involved,
//!   so a registration published through `<server>_connect` is visible on
//!   the very next completion even if that completion runs immediately.
//!
//! - The asynchronous watch below bumps a global epoch on every bucket
//!   change as a best-effort extra signal: it can invalidate a cached
//!   snapshot early, but by the time its event is delivered the revision
//!   check would have caught the same change anyway. It never *establishes*
//!   freshness; only the revision check does.
//!
//! Both are best-effort on failure: if the broker is unreachable, discovery
//! falls back to the plain 30-second TTL, which is the pre-change behavior.

use crate::config::Config;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use harnx_toolset_server::TOOL_REGISTRY_BUCKET;

/// How often the watcher retries attaching to a bucket that doesn't exist yet.
const WATCH_ATTACH_RETRY: Duration = Duration::from_millis(500);

/// The current registry epoch. Any change to the bucket bumps it, which
/// invalidates every cached discovery snapshot at once.
pub fn registry_epoch() -> u64 {
    REGISTRY_EPOCH.load(Ordering::Acquire)
}

static REGISTRY_EPOCH: AtomicU64 = AtomicU64::new(0);
static WATCH_RUNNING: AtomicBool = AtomicBool::new(false);

/// Ensure a registry watcher runs for this process, if it isn't already.
///
/// Cheap to call on every discovery: the fast path is one atomic load. The
/// watcher spans the whole bucket rather than one scope's prefix because a
/// single process only ever discovers tools against one cluster, and
/// over-invalidation costs one KV scan while a missed change would cost a
/// stale tool list for the full 30-second cache interval.
pub(crate) async fn ensure_registry_watch(config: &Config) {
    if WATCH_RUNNING.load(Ordering::Relaxed) {
        return;
    }
    if WATCH_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let cluster = config.default_cluster_key().to_string();
    let config = config.clone();
    tokio::spawn(async move {
        watch_loop(&config, &cluster).await;
        // The loop only returns when the config can no longer connect; let a
        // later discovery try again rather than disabling freshness forever.
        WATCH_RUNNING.store(false, Ordering::SeqCst);
    });
}

async fn watch_loop(config: &Config, cluster: &str) {
    loop {
        let client = match config.nats_client(cluster).await {
            Ok(client) => client,
            Err(error) => {
                log::debug!("tool registry watch connect failed: {error:#}");
                return;
            }
        };
        let jetstream = async_nats::jetstream::new(client);
        // A bucket that doesn't exist yet carries no registrations to be
        // stale about, but the first `ensure_bucket_with_ttl` by a tool
        // server can appear at any moment, so retry at a short interval.
        let store = loop {
            match jetstream.get_key_value(TOOL_REGISTRY_BUCKET).await {
                Ok(store) => break store,
                Err(error) => {
                    log::debug!(
                        "tool registry watch waiting for bucket \
                         '{TOOL_REGISTRY_BUCKET}': {error}"
                    );
                    tokio::time::sleep(WATCH_ATTACH_RETRY).await;
                }
            }
        };
        // The watcher yields an initial entry per existing key; those bumps
        // only cost the very next discovery one extra scan.
        let mut watcher = match store.watch_all().await {
            Ok(watcher) => watcher,
            Err(error) => {
                log::warn!("tool registry watch failed: {error:#}");
                tokio::time::sleep(WATCH_ATTACH_RETRY).await;
                continue;
            }
        };
        // The only way out of this inner loop is the stream ending (bucket
        // deleted) or the runtime shutting down; both detach the watch and
        // let the outer loop re-attach.
        while let Some(_entry) = watcher.next().await {
            REGISTRY_EPOCH.fetch_add(1, Ordering::Release);
        }
        log::debug!("tool registry watch stream ended; re-attaching");
    }
}

/// The registry bucket's latest stream sequence, for deterministic
/// next-completion freshness (decision 5b).
///
/// A KV write is acknowledged only after the registry stream records it, so
/// this sequence moves synchronously with every registration write and
/// delete — no event delivery involved. A discovery snapshot stores the
/// sequence it *started* scanning under, and the next completion compares
/// that stored sequence against this one; any mismatch forces a rescan.
/// This is what actually guarantees the "very next completion" rule: the
/// asynchronous watch above may not have delivered its event yet when the
/// next completion runs, but the broker has already committed the sequence.
///
/// Callers must pass an existing connection (the discovery transport's
/// client, or a cached provider's client). `Config::nats_client` opens a
/// fresh connection per call — hundreds of milliseconds on CI — so probing
/// through it on every completion multiplied session-start latency enough
/// to push delegated child claims past their inherited deadline (PR #2835
/// macOS CI failure).
///
/// `Ok(None)`: the registry stream doesn't exist, so no registrations exist
/// under any scope yet (callers treat that as sequence 0).
pub(crate) async fn registry_state_revision(
    jetstream: &async_nats::jetstream::Context,
) -> anyhow::Result<Option<u64>> {
    use async_nats::jetstream::{context::GetStreamErrorKind, ErrorCode};

    match jetstream
        .get_stream(format!("KV_{TOOL_REGISTRY_BUCKET}"))
        .await
    {
        // `get_stream` fetches the info with the same round trip; use the
        // cached copy rather than paying for a second request.
        Ok(stream) => Ok(Some(stream.cached_info().state.last_sequence)),
        Err(error)
            if matches!(
                error.kind(),
                GetStreamErrorKind::JetStream(ref error) if error.kind() == ErrorCode::STREAM_NOT_FOUND
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_counter_bumps_are_visible_to_readers() {
        harnx_core::require_nextest();
        let before = registry_epoch();
        REGISTRY_EPOCH.fetch_add(1, Ordering::Release);
        assert_eq!(registry_epoch(), before.wrapping_add(1));
        REGISTRY_EPOCH.fetch_sub(1, Ordering::Release);
        assert_eq!(registry_epoch(), before);
    }

    /// The three `registry_state_revision` outcomes a cached snapshot's
    /// freshness check distinguishes: absent stream (`Ok(None)`, callers read
    /// sequence 0 — the state that invalidates snapshots cached with a later
    /// revision when the whole bucket is deleted), present with a sequence
    /// (`Ok(Some)`), and a real transport failure (`Err`, probe maps to `None`
    /// and the epoch alone vouches for the snapshot).
    #[tokio::test]
    async fn nats_registry_state_revision_covers_absent_present_and_unreachable() {
        let Some((url, mut server, _store)) = crate::nats_worker::tests::spawn_test_nats().await
        else {
            return;
        };
        let client = async_nats::connect(&url)
            .await
            .expect("connect to test broker");
        let jetstream = async_nats::jetstream::new(client);

        // No registry stream exists yet: absent, not an error.
        assert_eq!(
            registry_state_revision(&jetstream)
                .await
                .expect("absent stream is not an error"),
            None
        );

        let bucket = jetstream
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: TOOL_REGISTRY_BUCKET.to_string(),
                ..Default::default()
            })
            .await
            .expect("create registry bucket");
        bucket
            .put("scope/server/tool", "registration".into())
            .await
            .expect("write registration");
        assert!(
            registry_state_revision(&jetstream)
                .await
                .expect("present stream is not an error")
                .is_some_and(|sequence| sequence >= 1),
            "a written registration moves the sequence past 0"
        );

        // Deleting the whole bucket returns the stream to absent — the state
        // a snapshot cached with a later revision must read as 0, not as an
        // error that falls back to the epoch-only path.
        jetstream
            .delete_key_value(TOOL_REGISTRY_BUCKET)
            .await
            .expect("delete registry bucket");
        assert_eq!(
            registry_state_revision(&jetstream)
                .await
                .expect("deleted stream is absent again"),
            None
        );

        // Broker gone: a real error, distinct from both states above. The
        // client buffers JetStream requests while it waits to reconnect, so
        // use a short-timeout context to observe the error quickly instead
        // of sitting out the default 10 s request timeout.
        server.kill().expect("kill test broker");
        let _ = server.wait();
        let quick = async_nats::jetstream::context::ContextBuilder::new()
            .timeout(std::time::Duration::from_secs(1))
            .build(jetstream.client().clone());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            registry_state_revision(&quick),
        )
        .await
        .expect("request against a dead broker fails within its context timeout");
        assert!(result.is_err(), "an unreachable broker is an error");
    }
}
