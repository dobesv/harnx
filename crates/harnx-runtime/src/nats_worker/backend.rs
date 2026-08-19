//! NATS session log backend for the async worker.

use crate::nats_lease::NatsSessionLease;
use crate::nats_metrics;
use anyhow::Result;
use async_nats::jetstream;
use std::sync::Arc;

const APPEND_ATTEMPTS: usize = 3;
const APPEND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

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

    fn failure_is_fatal(&self) -> bool {
        true
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
        let mut fenced = entry.clone();
        fenced.set_fence_token(self.lease.fence_token());
        self.backend
            .append_event_blocking_with_lease(&fenced, Some(&self.lease))
    }

    fn failure_is_fatal(&self) -> bool {
        true
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

    /// Append an entry.
    ///
    /// Prefer this over [`Self::append_event_blocking`] wherever the caller is
    /// already async.
    pub async fn append_event(&self, entry: &harnx_core::session::SessionLogEntry) -> Result<u64> {
        self.append_event_with_lease(entry, None).await
    }

    async fn append_event_with_lease(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        lease: Option<&NatsSessionLease>,
    ) -> Result<u64> {
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        let message_id = uuid::Uuid::new_v4().to_string();
        let mut last_error = None;
        let mut appended_seq = None;
        for attempt in 1..=APPEND_ATTEMPTS {
            if let Some(lease) = lease.filter(|lease| !lease.is_held()) {
                nats_metrics::fenced_write_rejected();
                warn!(
                    "fenced write rejected: session_id={} worker_id={} revision={} entry_type={}",
                    self.session_id(),
                    lease.worker_id(),
                    lease.fence_token(),
                    crate::session_history::entry_type(entry)
                );
                anyhow::bail!(
                    "refusing worker-originated append: session lease not held (fenced out)"
                );
            }

            match log
                .append_event_with_message_id_async(entry, message_id.clone())
                .await
            {
                Ok(seq) => {
                    appended_seq = Some(seq);
                    break;
                }
                Err(error) => {
                    if attempt < APPEND_ATTEMPTS {
                        warn!(
                            "retrying session append: session_id={} entry_type={} attempt={}/{} error={error:#}",
                            self.session_id(),
                            crate::session_history::entry_type(entry),
                            attempt,
                            APPEND_ATTEMPTS,
                        );
                    }
                    last_error = Some(error);
                    if attempt < APPEND_ATTEMPTS {
                        tokio::time::sleep(APPEND_RETRY_DELAY).await;
                    }
                }
            }
        }
        let seq = appended_seq.ok_or_else(|| {
            last_error.expect("at least one NATS append attempt must record an error")
        })?;
        self.observe_append(seq);
        Ok(seq)
    }

    /// Append an entry, blocking on the async NATS call.
    ///
    /// Must be called from within a Tokio multi-threaded runtime.
    /// Uses `tokio::task::block_in_place` to escape the async context.
    pub fn append_event_blocking(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> Result<u64> {
        self.append_event_blocking_with_lease(entry, None)
    }

    fn append_event_blocking_with_lease(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        lease: Option<&NatsSessionLease>,
    ) -> Result<u64> {
        let seq = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.append_event_with_lease(entry, lease))
        })?;
        Ok(seq)
    }

    /// Advance the P4.1 fan-out `after_seq` so subsequent advisories in this
    /// (possibly multi-step) turn carry an up-to-date durable sequence.
    fn observe_append(&self, seq: u64) {
        if let Some(observer) = &self.after_seq_observer {
            observer.fetch_max(seq, std::sync::atomic::Ordering::Relaxed);
        }
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
    /// re-read so the worker sees its own just-written completion boundary and
    /// does not re-fold already-answered messages.
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
