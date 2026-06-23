//! Async session log persistence for NATS-backed sessions.
//!
//! See the worker entrypoint in `nats_worker.rs` for orchestration.

use anyhow::Result;
use async_nats::jetstream;
use harnx_core::session::SessionLogEntry;

/// NATS JetStream-backed async session log.
///
/// Wraps `NatsSessionLog` async methods for use in the NATS worker.
/// Unlike the sync `SessionLog` trait, these methods are designed for
/// async contexts (Tokio runtime).
pub struct AsyncSessionLog {
    inner: crate::nats_session_log::NatsSessionLog,
}

impl AsyncSessionLog {
    /// Create a new async session log backed by NATS JetStream.
    pub fn new(jetstream: jetstream::Context, session_id: impl Into<String>) -> Self {
        Self {
            inner: crate::nats_session_log::NatsSessionLog::new(jetstream, session_id),
        }
    }

    /// Append an entry to the log, returning the JetStream sequence number.
    pub async fn append_event(&self, entry: &SessionLogEntry) -> Result<u64> {
        self.inner.append_event_async(entry).await
    }

    /// Load all entries from the session log.
    pub async fn load_events(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        self.inner.load_events_async().await
    }

    /// Replay entries from the given sequence number.
    pub async fn replay_from(&self, seq: u64) -> Result<Vec<SessionLogEntry>> {
        self.inner.replay_from_async(seq).await
    }

    /// Load and replay the session into an in-memory `Session`.
    pub async fn load_session(&self, name: &str) -> Result<harnx_core::session::Session> {
        let entries = self.load_events().await?;
        crate::nats_session_log::load_session_from_entries(&entries, name)
    }
}
