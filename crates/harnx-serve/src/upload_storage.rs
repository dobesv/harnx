//! Server-owned upload transport cache: one JetStream context plus a
//! resettable bucket-ensured memo per resolved cluster, so repeated uploads
//! reuse a broker connection and bucket handle instead of reconnecting and
//! re-ensuring on every request.
//!
//! The cache lives for the `Server`'s lifetime. The local managed broker
//! binds a persistent endpoint (port and token come from the runtime ports
//! file and failover rebinds the original port), and named clusters come
//! from the config snapshot taken at startup, so no endpoint or credential
//! can change under a cached connection without a server rebuild.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use harnx_blob_store::media::AttachmentsBucket;

/// How long a cached bucket serves before the next upload re-ensures it.
/// `ensure_attachments_bucket` reconciles replicas best-effort; without a
/// recheck, a raise that failed once would stay unretried for as long as the
/// memo lives. This bounds the retry rate.
pub(crate) const DEFAULT_BUCKET_RECHECK_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);

/// One resolved cluster's shared upload transport. The bucket memo sits
/// beside the connection so a deleted stream can reset it without losing
/// the connection.
pub(crate) struct ClusterStorage {
    pub(crate) jetstream: async_nats::jetstream::Context,
    pub(crate) client: async_nats::Client,
    /// The ensured bucket handle, or `None` when the memo is reset and the
    /// next upload must re-ensure the bucket on the cached connection.
    pub(crate) bucket: Option<AttachmentsBucket>,
    /// When the bucket was last ensured; read against the server's recheck
    /// interval to bound the re-ensure rate.
    pub(crate) ensured_at: Instant,
}

impl ClusterStorage {
    /// One cluster's cached transport. `bucket` is the ensured bucket handle,
    /// or `None` while the memo is reset and the next upload must re-ensure.
    pub(crate) fn new(
        jetstream: async_nats::jetstream::Context,
        bucket: Option<AttachmentsBucket>,
    ) -> Self {
        let client = jetstream.client();
        Self {
            jetstream,
            client,
            bucket,
            ensured_at: Instant::now(),
        }
    }
}

/// Per-cluster lazily initialized state, single-flighted by the `storage`
/// lock: the first upload to a cluster connects and ensures while later
/// uploads clone the cached handles. No put takes this lock, so uploads
/// never serialize behind each other.
pub(crate) struct ClusterEntry {
    pub(crate) storage: tokio::sync::Mutex<Option<ClusterStorage>>,
    /// Successful connects to this cluster; the test seam for
    /// connection-reuse assertions.
    pub(crate) connects: AtomicUsize,
    /// Bucket ensures on this cluster; the test seam for ensure-reuse and
    /// recheck-rate assertions.
    pub(crate) ensures: AtomicUsize,
}

/// Per-resolved-cluster cache. The map lock only creates or looks up the
/// per-cluster entry; no lock here is held across NATS I/O — only the
/// single-flight init lock of one cluster can cover I/O, so a slow cluster
/// never blocks another.
#[derive(Default)]
pub(crate) struct UploadStorage {
    clusters: tokio::sync::Mutex<HashMap<String, Arc<ClusterEntry>>>,
}

impl UploadStorage {
    /// The entry for one cluster, created on first use.
    pub(crate) async fn entry(&self, cluster: &str) -> Arc<ClusterEntry> {
        self.clusters
            .lock()
            .await
            .entry(cluster.to_string())
            .or_insert_with(|| {
                Arc::new(ClusterEntry {
                    storage: tokio::sync::Mutex::new(None),
                    connects: AtomicUsize::new(0),
                    ensures: AtomicUsize::new(0),
                })
            })
            .clone()
    }
}

/// Bump the successful-connect counter (test seam for reuse assertions).
pub(crate) fn count_connect(entry: &ClusterEntry) {
    entry.connects.fetch_add(1, Ordering::Relaxed);
}

/// Bump the bucket-ensure counter (test seam for reuse assertions).
pub(crate) fn count_ensure(entry: &ClusterEntry) {
    entry.ensures.fetch_add(1, Ordering::Relaxed);
}

/// Test seam for the upload-delete race: when armed, a part that was just
/// acknowledged parks here, before its session recheck, until the test
/// releases it — so the test can delete the session (or kill the broker)
/// exactly between the put commit and the recheck. Unarmed, `wait` is a
/// lock-free atomic check. Production never arms it.
#[derive(Default)]
pub(crate) struct UploadRecheckGate {
    /// Fast-path flag: unarmed parts never touch the mutex below.
    armed: std::sync::atomic::AtomicBool,
    /// The armed gate: parts count themselves in `arrivals` and park until
    /// `released` turns true.
    state: tokio::sync::Mutex<Option<GateArm>>,
}

struct GateArm {
    arrivals: std::sync::Arc<AtomicUsize>,
    released: tokio::sync::watch::Receiver<bool>,
}

impl UploadRecheckGate {
    pub(crate) async fn wait(&self) {
        if !self.armed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let (arrivals, mut released) = match self.state.lock().await.as_ref() {
            Some(arm) => (std::sync::Arc::clone(&arm.arrivals), arm.released.clone()),
            None => return,
        };
        arrivals.fetch_add(1, Ordering::Relaxed);
        while !*released.borrow() {
            if released.changed().await.is_err() {
                return;
            }
        }
    }

    /// Arm the gate; returns the arrival counter and the release switch.
    /// Only tests arm the gate.
    #[cfg(test)]
    pub(crate) async fn arm(
        &self,
    ) -> (
        std::sync::Arc<AtomicUsize>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let (release, released) = tokio::sync::watch::channel(false);
        let arrivals = std::sync::Arc::new(AtomicUsize::new(0));
        self.state.lock().await.get_or_insert_with(|| GateArm {
            arrivals: std::sync::Arc::clone(&arrivals),
            released,
        });
        // Armed last: a part that passes the atomic check is guaranteed to
        // find the arm below it.
        self.armed.store(true, std::sync::atomic::Ordering::Release);
        (arrivals, release)
    }
}
