//! NATS session log backend for the async worker.

use crate::nats_lease::NatsSessionLease;
use crate::nats_metrics;
use crate::nats_session_metadata::MetadataOutput;
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
    execution: Option<crate::execution_fence::GenerationFence>,
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

    fn validate_output(&self) -> Result<()> {
        if let Some(fence) = &self.execution {
            fence.check_blocking("transcript-reducer")?;
        }
        Ok(())
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

/// Worker sink bound to one execution generation and lease owner.
///
/// The backend commits exact output through the tree gate before its private
/// ordered projector appends. Local lease checks are fast rejects only; owner
/// handover and stop share the same gate CAS as output. A valid lease cannot
/// authorize an old generation's output.
///
/// HITL also retains the expected stream tail used to derive the decision. A
/// conditional projection that loses forces re-derivation. The continuation
/// still revalidates lease ownership immediately before approved tool dispatch.
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

    /// Append one HITL control entry only if the session log tail still matches
    /// the snapshot used to derive it. `None` means another writer advanced the
    /// stream and won the race.
    pub async fn append_hitl_event_cas(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        expected_last_sequence: u64,
    ) -> Result<Option<u64>> {
        self.require_generation()?;
        let fenced = self.fenced_entry(entry);
        self.backend
            .append_event_with_expected_last_sequence_and_lease(
                &fenced,
                expected_last_sequence,
                &self.lease,
            )
            .await
    }

    /// Blocking form used by the synchronous agent-loop approval callback.
    pub fn append_hitl_event_cas_blocking(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        expected_last_sequence: u64,
    ) -> Result<Option<u64>> {
        let sink = self.clone();
        let entry = entry.clone();
        crate::execution_fence::block_on_io(async move {
            sink.append_hitl_event_cas(&entry, expected_last_sequence)
                .await
        })
    }

    fn require_generation(&self) -> Result<()> {
        anyhow::ensure!(
            self.backend.execution.is_some(),
            "worker output requires execution generation authority"
        );
        Ok(())
    }

    fn fenced_entry(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> harnx_core::session::SessionLogEntry {
        use harnx_core::session::SessionLogEntry;
        let mut entry = entry.clone();
        let revision = self.lease.fence_token();
        match &mut entry {
            SessionLogEntry::Cancel { fence_token }
            | SessionLogEntry::Error { fence_token, .. }
            | SessionLogEntry::TurnEnd { fence_token, .. } => *fence_token = revision,
            _ => entry.set_fence_token(revision),
        }
        entry
    }

    fn persist_metadata(&self, replacement: MetadataReplacement) -> Result<()> {
        self.require_generation()?;
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
        self.require_generation()?;
        let fenced = self.fenced_entry(entry);
        self.backend
            .append_event_blocking_with_lease(&fenced, Some(&self.lease))
    }

    fn validate_output(&self) -> Result<()> {
        if let Some(fence) = &self.backend.execution {
            fence.check_blocking("transcript-reducer")?;
        }
        Ok(())
    }

    fn failure_is_fatal(&self) -> bool {
        true
    }

    fn persist_title(&self, title: &str, manual: bool, tokens: usize) -> Result<()> {
        self.require_generation()?;
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
            self.require_generation()?;
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
            execution: None,
            after_seq_observer: None,
            metadata_store: None,
        }
    }

    pub fn with_execution(
        mut self,
        execution: Option<crate::execution_fence::GenerationFence>,
    ) -> Self {
        self.execution = execution;
        self
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

    /// Returns the metadata store if attached, for optional access without error propagation.
    pub fn metadata_store_opt(
        &self,
    ) -> Option<&crate::nats_session_metadata::SessionMetadataStore> {
        self.metadata_store.as_ref()
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
        if self.project_metadata_blocking(MetadataOutput::Title {
            title: title.clone(),
            manual: update.manual,
            tokens: update.tokens,
        })? {
            return Ok(());
        }
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
        let output = match &replacement {
            MetadataReplacement::Overrides(value) => MetadataOutput::Overrides(value.clone()),
            MetadataReplacement::Override(value) => MetadataOutput::Override(value.clone()),
            MetadataReplacement::Variables(value) => MetadataOutput::Variables(value.clone()),
        };
        if self.project_metadata_blocking(output)? {
            return Ok(());
        }
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
        if self
            .project_metadata(MetadataOutput::ExecutionContexts(observations.to_vec()))
            .await?
        {
            return Ok(());
        }
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

    fn project_metadata_blocking(&self, output: MetadataOutput) -> Result<bool> {
        if self.execution.is_none() {
            return Ok(false);
        }
        let backend = self.clone();
        crate::execution_fence::block_on_io(async move { backend.project_metadata(output).await })
    }

    async fn project_metadata(&self, output: MetadataOutput) -> Result<bool> {
        let Some(fence) = &self.execution else {
            return Ok(false);
        };
        self.metadata_store()?;
        let receipt = fence
            .output(
                harnx_execution_control::OutputKind::SessionMetadata,
                serde_json::to_value(output)?,
            )
            .await?;
        crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        )
        .project_through(fence, &receipt)
        .await?;
        Ok(true)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Reconcile attention sequence from the log.
    ///
    /// CAS-bumps `last_attention_seq = max(current, derive_attention_seq(entries))`.
    /// Idempotent; only writes/publishes on change. Used to repair lost bumps.
    pub async fn reconcile_attention_from_log(
        &self,
        entries: &[(u64, harnx_core::session::SessionLogEntry)],
    ) -> Result<()> {
        let Some(store) = self.metadata_store.as_ref().cloned() else {
            // No metadata store attached - nothing to reconcile
            return Ok(());
        };
        let attention_seq = super::agent_loop::derive_attention_seq(entries);
        if attention_seq == 0 {
            // No attention-producing entries
            return Ok(());
        }
        if let Err(error) = store.bump_attention(&self.session_id, attention_seq).await {
            log::warn!(
                "failed to reconcile attention seq from log: session_id={} seq={} error={error:#}",
                self.session_id,
                attention_seq
            );
        }
        Ok(())
    }

    /// Append an entry.
    ///
    /// Prefer this over [`Self::append_event_blocking`] wherever the caller is
    /// already async.
    pub async fn append_event(&self, entry: &harnx_core::session::SessionLogEntry) -> Result<u64> {
        self.append_event_with_lease(entry, None).await
    }

    pub(crate) async fn append_event_with_expected_last_sequence_and_lease(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        expected_last_sequence: u64,
        lease: &NatsSessionLease,
    ) -> Result<Option<u64>> {
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        let fence = self
            .execution
            .as_ref()
            .context("worker append requires execution generation authority")?;
        self.ensure_lease_held(lease, entry)?;
        let seq = log
            .append_output(fence, entry, Some(expected_last_sequence))
            .await?;
        if let Some(seq) = seq {
            self.observe_append(seq);
        }
        Ok(seq)
    }

    fn ensure_lease_held(
        &self,
        lease: &NatsSessionLease,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> Result<()> {
        if lease.is_held() {
            if let Some(fence) = &self.execution {
                // Renewals advance the audit revision, not the gate owner captured at claim.
                anyhow::ensure!(
                    fence.context.generation_owner().instance_id == lease.worker_id()
                        && fence.context.generation_owner().fence <= lease.fence_token(),
                    "sink lease does not own execution generation"
                );
            }
            return Ok(());
        }
        nats_metrics::fenced_write_rejected();
        warn!(
            "fenced write rejected: session_id={} worker_id={} revision={} entry_type={}",
            self.session_id(),
            lease.worker_id(),
            lease.fence_token(),
            crate::session_history::entry_type(entry)
        );
        anyhow::bail!("refusing worker-originated append: session lease not held (fenced out)")
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
        if let Some(lease) = lease {
            self.ensure_lease_held(lease, entry)?;
        }
        if let Some(fence) = &self.execution {
            return self.append_committed(&log, fence, entry).await;
        }
        self.append_control_entry(&log, entry).await
    }

    async fn append_control_entry(
        &self,
        log: &crate::nats_session_log::NatsSessionLog,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> Result<u64> {
        anyhow::ensure!(
            matches!(
                entry,
                harnx_core::session::SessionLogEntry::Cancel { .. }
                    | harnx_core::session::SessionLogEntry::Message {
                        role: harnx_core::message::MessageRole::User,
                        ..
                    }
            ),
            "worker output requires execution generation authority"
        );
        let message_id = uuid::Uuid::new_v4().to_string();
        let mut last_error = None;
        let mut appended_seq = None;
        for attempt in 1..=APPEND_ATTEMPTS {
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

    async fn append_committed(
        &self,
        log: &crate::nats_session_log::NatsSessionLog,
        fence: &crate::execution_fence::GenerationFence,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> Result<u64> {
        let seq = if let harnx_core::session::SessionLogEntry::Cancel { fence_token } = entry {
            let entries = log.load_events_latest_async().await?;
            log.append_cancellation(
                fence,
                (entries.last().map_or(0, |(seq, _)| *seq), *fence_token),
            )
            .await?
            .context("cancel projection superseded")?
        } else {
            log.append_output(fence, entry, None)
                .await?
                .context("transcript projection missing")?
        };
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
        lease: Option<&Arc<NatsSessionLease>>,
    ) -> Result<u64> {
        if let Some(lease) = lease {
            self.ensure_lease_held(lease, entry)?;
        }
        let backend = self.clone();
        let entry = entry.clone();
        let lease = lease.cloned();
        crate::execution_fence::block_on_io(async move {
            backend
                .append_event_with_lease(&entry, lease.as_deref())
                .await
        })
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

#[cfg(test)]
pub(crate) async fn test_generation_fence(
    jetstream: &jetstream::Context,
    session_id: &str,
    lease: &NatsSessionLease,
) -> crate::execution_fence::GenerationFence {
    let store = harnx_execution_control::ExecutionStore::ensure(jetstream, 1)
        .await
        .unwrap();
    let operation = store.session(session_id, None, None).await.unwrap();
    store
        .claim(
            &operation.reference,
            harnx_execution_control::Owner {
                instance_id: lease.worker_id().into(),
                fence: lease.fence_token(),
            },
        )
        .await
        .unwrap();
    let context = store.activate_gate(&operation.reference).await.unwrap();
    crate::execution_fence::GenerationFence::new(store, context)
}

#[cfg(test)]
pub(crate) async fn test_session_authority(
    jetstream: &jetstream::Context,
    session_id: &str,
    store: &crate::nats_session_metadata::SessionMetadataStore,
) -> (
    Arc<NatsSessionLease>,
    crate::execution_fence::GenerationFence,
) {
    use crate::nats_lease::{NatsLeaseAcquireParams, NatsLeaseConfig};
    let lease = NatsSessionLease::acquire(NatsLeaseAcquireParams {
        jetstream: jetstream.clone(),
        session_id,
        worker_id: "test-worker".to_string(),
        generation: 1,
        config: NatsLeaseConfig {
            ttl: std::time::Duration::from_secs(5),
            renew_interval: std::time::Duration::from_millis(500),
            replicas: 1,
            ..Default::default()
        },
        session_metadata: Some(store.clone()),
    })
    .await
    .unwrap()
    .expect("lease should be acquired");
    let lease = Arc::new(lease);
    let fence = test_generation_fence(jetstream, session_id, &lease).await;
    (lease, fence)
}
