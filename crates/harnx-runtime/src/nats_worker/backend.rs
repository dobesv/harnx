//! NATS session log backend for the async worker.

use crate::nats_lease::NatsSessionLease;
use crate::nats_metrics;
use anyhow::Result;
use async_nats::jetstream;
use std::sync::Arc;

/// NATS session log backend for the async worker.
///
/// Wraps `NatsSessionLog` and provides blocking entrypoints for the sync
/// persistence path used by `run_agent_loop`.
#[derive(Clone)]
pub struct NatsSessionLogBackend {
    jetstream: jetstream::Context,
    session_id: String,
    /// Optional observer of the latest durable append sequence. When set, every
    /// successful append advances it via `fetch_max`, so the live-event fan-out
    /// sink (P4.1) can stamp advisories with an up-to-date `after_seq` during
    /// multi-step turns WITHOUT a per-event JetStream query.
    after_seq_observer: Option<Arc<std::sync::atomic::AtomicU64>>,
}

impl crate::config::session::SessionAppendSink for NatsSessionLogBackend {
    fn append(&self, entry: &harnx_core::session::SessionLogEntry) -> Result<u64> {
        self.append_event_blocking(entry)
    }
}

/// Fence-guarded append sink for HA worker writes (P2.2).
///
/// Wraps a [`NatsSessionLogBackend`] with the holding [`NatsSessionLease`].
/// Before EVERY worker-originated append it verifies `lease.is_held()` (mutual
/// exclusion: a worker that has lost its lease must not write), and it stamps
/// `fence_token = lease.fence_token()` on entries that carry one
/// (`Message`/`ToolCalls`). A stale worker therefore cannot append, and a newer
/// worker can detect stale entries via the fence on resume.
#[derive(Clone)]
pub struct FencedSessionLogSink {
    backend: NatsSessionLogBackend,
    lease: Arc<NatsSessionLease>,
}

impl FencedSessionLogSink {
    pub fn new(backend: NatsSessionLogBackend, lease: Arc<NatsSessionLease>) -> Self {
        Self { backend, lease }
    }
}

impl crate::config::session::SessionAppendSink for FencedSessionLogSink {
    fn append(&self, entry: &harnx_core::session::SessionLogEntry) -> Result<u64> {
        if !self.lease.is_held() {
            nats_metrics::fenced_write_rejected();
            warn!(
                "fenced write rejected: session_id={} worker_id={} revision={} entry_type={}",
                self.backend.session_id(),
                self.lease.worker_id(),
                self.lease.fence_token(),
                crate::session_history::entry_type(entry)
            );
            anyhow::bail!("refusing worker-originated append: session lease not held (fenced out)");
        }
        let mut fenced = entry.clone();
        fenced.set_fence_token(self.lease.fence_token());
        self.backend.append_event_blocking(&fenced)
    }
}

impl NatsSessionLogBackend {
    pub fn new(jetstream: jetstream::Context, session_id: impl Into<String>) -> Self {
        Self {
            jetstream,
            session_id: session_id.into(),
            after_seq_observer: None,
        }
    }

    pub fn jetstream(&self) -> jetstream::Context {
        self.jetstream.clone()
    }

    /// Attach an observer that tracks the latest durable append sequence
    /// (advanced via `fetch_max` on every successful append). Used to keep the
    /// P4.1 live-event sink's `after_seq` current during multi-step turns.
    pub fn with_after_seq_observer(mut self, observer: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.after_seq_observer = Some(observer);
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Append an entry, blocking on the async NATS call.
    ///
    /// Must be called from within a Tokio multi-threaded runtime.
    /// Uses `tokio::task::block_in_place` to escape the async context.
    pub fn append_event_blocking(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> Result<u64> {
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        let seq = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(log.append_event_async(entry))
        })?;
        // Advance the P4.1 fan-out `after_seq` so subsequent advisories in this
        // (possibly multi-step) turn carry an up-to-date durable sequence.
        if let Some(observer) = &self.after_seq_observer {
            observer.fetch_max(seq, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(seq)
    }

    /// Load all events, blocking on async NATS reads.
    pub fn load_events_blocking(&self) -> Result<Vec<(u64, harnx_core::session::SessionLogEntry)>> {
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(log.load_events_async())
        })
    }

    /// Load all events with read-your-writes consistency: wait (bounded) until
    /// the stream reflects at least the worker's latest durable append before
    /// reading. Uses the shared `after_seq_observer` high-water mark when set;
    /// falls back to a plain load otherwise. Used by the end-of-turn drain
    /// re-read so the worker sees its own just-written turn barrier and does not
    /// re-fold already-answered messages.
    pub async fn load_events_consistent_async(
        &self,
    ) -> Result<Vec<(u64, harnx_core::session::SessionLogEntry)>> {
        let min_seq = self
            .after_seq_observer
            .as_ref()
            .map(|o| o.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        log.load_events_at_least_async(min_seq).await
    }

    pub async fn load_events_latest_async(
        &self,
    ) -> Result<Vec<(u64, harnx_core::session::SessionLogEntry)>> {
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        log.load_events_latest_async().await
    }
}
