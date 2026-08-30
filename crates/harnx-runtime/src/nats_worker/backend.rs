//! NATS session log backend for the async worker.

use crate::nats_lease::NatsSessionLease;
use crate::nats_metrics;
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::execution_context::ExecutionContextObservation;
use std::sync::Arc;

const APPEND_ATTEMPTS: usize = 3;
const APPEND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

enum MetadataReplacement {
    Overrides(crate::nats_session_metadata::SessionOverrides),
    Override(crate::nats_session_metadata::SessionOverrideUpdate),
    Variables(harnx_core::agent_config::AgentVariables),
}

struct TitleUpdate<'a> {
    title: &'a str,
    manual: bool,
    tokens: usize,
}

impl MetadataReplacement {
    fn apply(&self, metadata: &mut crate::nats_session_metadata::SessionMetadata) {
        match self {
            Self::Overrides(overrides) => metadata.overrides = overrides.clone(),
            Self::Override(update) => update.apply(&mut metadata.overrides),
            Self::Variables(variables) => metadata.variables = variables.clone(),
        }
    }
}

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
    metadata_store: Option<crate::nats_session_metadata::SessionMetadataStore>,
}

impl crate::config::session::SessionAppendSink for NatsSessionLogBackend {
    fn append(&self, entry: &harnx_core::session::SessionLogEntry) -> Result<u64> {
        self.append_event_blocking(entry)
    }

    fn failure_is_fatal(&self) -> bool {
        true
    }

    fn persist_title(&self, title: &str, manual: bool, tokens: usize) -> Result<()> {
        self.persist_title_blocking(
            TitleUpdate {
                title,
                manual,
                tokens,
            },
            None,
        )
    }

    fn persist_overrides(
        &self,
        overrides: &crate::nats_session_metadata::SessionOverrides,
    ) -> Result<()> {
        self.persist_metadata_blocking(MetadataReplacement::Overrides(overrides.clone()), None)
    }

    fn persist_override(
        &self,
        update: &crate::nats_session_metadata::SessionOverrideUpdate,
    ) -> Result<()> {
        self.persist_metadata_blocking(MetadataReplacement::Override(update.clone()), None)
    }

    fn persist_variables(
        &self,
        variables: &harnx_core::agent_config::AgentVariables,
    ) -> Result<()> {
        self.persist_metadata_blocking(MetadataReplacement::Variables(variables.clone()), None)
    }

    fn persist_execution_contexts<'a>(
        &'a self,
        observations: &'a [ExecutionContextObservation],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.persist_execution_contexts_async(observations, None)
                .await
        })
    }

    fn load_overrides(&self) -> Result<Option<crate::nats_session_metadata::SessionOverrides>> {
        self.load_overrides_blocking()
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

    pub fn with_metadata_store(
        mut self,
        store: Option<crate::nats_session_metadata::SessionMetadataStore>,
    ) -> Self {
        self.backend = self.backend.with_metadata_store(store);
        self
    }

    fn persist_metadata(&self, replacement: MetadataReplacement) -> Result<()> {
        anyhow::ensure!(
            self.lease.is_held(),
            "session lease lost before metadata update"
        );
        self.backend
            .persist_metadata_blocking(replacement, Some(self.lease.fence_token()))
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

    fn persist_title(&self, title: &str, manual: bool, tokens: usize) -> Result<()> {
        anyhow::ensure!(
            self.lease.is_held(),
            "session lease lost before title update"
        );
        self.backend.persist_title_blocking(
            TitleUpdate {
                title,
                manual,
                tokens,
            },
            Some(self.lease.fence_token()),
        )
    }

    fn persist_overrides(
        &self,
        overrides: &crate::nats_session_metadata::SessionOverrides,
    ) -> Result<()> {
        self.persist_metadata(MetadataReplacement::Overrides(overrides.clone()))
    }

    fn persist_override(
        &self,
        update: &crate::nats_session_metadata::SessionOverrideUpdate,
    ) -> Result<()> {
        self.persist_metadata(MetadataReplacement::Override(update.clone()))
    }

    fn persist_variables(
        &self,
        variables: &harnx_core::agent_config::AgentVariables,
    ) -> Result<()> {
        self.persist_metadata(MetadataReplacement::Variables(variables.clone()))
    }

    fn persist_execution_contexts<'a>(
        &'a self,
        observations: &'a [ExecutionContextObservation],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            anyhow::ensure!(
                self.lease.is_held(),
                "session lease lost before execution-context update"
            );
            self.backend
                .persist_execution_contexts_async(observations, Some(self.lease.fence_token()))
                .await
        })
    }

    fn load_overrides(&self) -> Result<Option<crate::nats_session_metadata::SessionOverrides>> {
        self.backend.load_overrides_blocking()
    }
}

impl NatsSessionLogBackend {
    pub fn new(jetstream: jetstream::Context, session_id: impl Into<String>) -> Self {
        Self {
            jetstream,
            session_id: session_id.into(),
            after_seq_observer: None,
            metadata_store: None,
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

    pub fn with_metadata_store(
        mut self,
        store: Option<crate::nats_session_metadata::SessionMetadataStore>,
    ) -> Self {
        self.metadata_store = store;
        self
    }

    fn metadata_store(&self) -> Result<&crate::nats_session_metadata::SessionMetadataStore> {
        self.metadata_store
            .as_ref()
            .context("canonical session metadata store is not attached")
    }

    fn patch_metadata_blocking<F>(&self, fence_token: Option<u64>, patch: F) -> Result<()>
    where
        F: FnMut(&mut crate::nats_session_metadata::SessionMetadata) -> Result<()>,
    {
        let store = self.metadata_store()?.clone();
        let session_id = self.session_id.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                if let Some(fence_token) = fence_token {
                    store
                        .patch_with_fence(&session_id, fence_token, patch)
                        .await
                } else {
                    store.patch(&session_id, patch).await
                }
            })
        })?;
        Ok(())
    }

    fn persist_title_blocking(
        &self,
        update: TitleUpdate<'_>,
        fence_token: Option<u64>,
    ) -> Result<()> {
        let title = update.title.to_string();
        self.patch_metadata_blocking(fence_token, move |metadata| {
            metadata.title.value = Some(title.clone());
            metadata.title.manual = update.manual;
            metadata.title.last_updated_tokens = update.tokens;
            Ok(())
        })
    }

    fn persist_metadata_blocking(
        &self,
        replacement: MetadataReplacement,
        fence_token: Option<u64>,
    ) -> Result<()> {
        self.patch_metadata_blocking(fence_token, move |metadata| {
            replacement.apply(metadata);
            Ok(())
        })
    }

    fn load_overrides_blocking(
        &self,
    ) -> Result<Option<crate::nats_session_metadata::SessionOverrides>> {
        let Some(store) = self.metadata_store.as_ref().cloned() else {
            return Ok(None);
        };
        let session_id = self.session_id.clone();
        let record = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(store.get(&session_id))
        })?
        .with_context(|| format!("canonical session metadata '{session_id}' not found"))?;
        Ok(Some(record.metadata.overrides))
    }

    async fn persist_execution_contexts_async(
        &self,
        observations: &[ExecutionContextObservation],
        fence_token: Option<u64>,
    ) -> Result<()> {
        let store = self.metadata_store()?;
        if let Some(fence_token) = fence_token {
            store
                .merge_execution_contexts_with_fence(&self.session_id, fence_token, observations)
                .await?;
        } else {
            store
                .merge_execution_contexts(&self.session_id, observations)
                .await?;
        }
        Ok(())
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
