//! NATS session log backend for the async worker.

use crate::nats_lease::NatsSessionLease;
use crate::nats_metrics;
use anyhow::{Context, Result};
use async_nats::jetstream;
use harnx_core::execution_context::ExecutionContextObservation;
use std::sync::Arc;

/// Run an async NATS round trip from a blocking persistence callback without
/// wedging the Tokio worker thread it is on. The agent loop persists model and
/// tool output through deeply synchronous code, so the broker call has to be
/// handed to the runtime rather than awaited in place. The join handle is
/// owned, so dropping the waiter aborts the task instead of detaching
/// unfinished I/O.
fn block_on_io<T: Send + 'static>(
    future: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    use tracing::Instrument;
    let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
        future.instrument(tracing::Span::current()),
    ));
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(task))?
}

const APPEND_ATTEMPTS: usize = 3;

const APPEND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// How many times a fenced append re-reads a moved tail before giving up.
/// Only writers this worker is allowed to append behind (queued user input,
/// and for wind-up another `Cancel`) move it, so a handful of rounds is
/// generous; the bound is there so a pathological writer cannot livelock the
/// worker.
const FENCED_APPEND_ATTEMPTS: usize = 8;

/// Which writer's conflict rule a fenced append follows.
#[derive(Clone, Copy)]
enum WriterRule {
    /// The turn's own writer. A newer `Cancel` has ended the turn, so its
    /// entry is abandoned rather than written behind the interruption.
    Turn,
    /// Wind-up of an interrupted turn. It is idempotent and owes the log a
    /// result either way, so it appends behind a second `Cancel` too.
    WindUp,
}

/// One fenced append: where the writer expects the log to end, which rule it
/// follows when something else got there first, and the identity JetStream
/// deduplicates the publish by.
struct FencedWrite {
    expected_tail: u64,
    rule: WriterRule,
    message_id: String,
}

/// The interrupted round a wind-up is closing out: the `Cancel` that ended it,
/// and where the writer expects the log to end.
pub(crate) struct WoundUpRound {
    pub cancel_seq: u64,
    pub expected_tail: u64,
}

impl WoundUpRound {
    /// The publish identity every worker winding this round up shares, so a
    /// replacement that takes over after a lease handover is deduplicated by
    /// the broker rather than appending a second answer to a round the log
    /// has already closed.
    fn message_id(&self, session_id: &str) -> String {
        format!("windup-{session_id}-{}", self.cancel_seq)
    }
}

/// A newer `Cancel` terminated the turn while its writer was appending.
/// Callers recognise it with `error.is::<TurnInterrupted>()` and stop writing
/// instead of replaying the entry behind the interruption.
#[derive(Debug)]
pub(crate) struct TurnInterrupted {
    pub cancel_seq: u64,
}

impl std::fmt::Display for TurnInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "turn interrupted by a Cancel at sequence {}",
            self.cancel_seq
        )
    }
}

impl std::error::Error for TurnInterrupted {}

/// Stamp `revision` onto whichever fence field the entry carries. `Cancel`,
/// `Error` and `TurnEnd` always carry one; `Message` and `ToolCalls` carry an
/// optional one; everything else is unfenced and left alone.
fn stamp_fence_token(
    entry: &harnx_core::session::SessionLogEntry,
    revision: u64,
) -> harnx_core::session::SessionLogEntry {
    use harnx_core::session::SessionLogEntry;
    let mut entry = entry.clone();
    match &mut entry {
        SessionLogEntry::Cancel { fence_token, .. }
        | SessionLogEntry::Error { fence_token, .. }
        | SessionLogEntry::TurnEnd { fence_token, .. } => *fence_token = revision,
        _ => entry.set_fence_token(revision),
    }
    entry
}

/// What a fenced append does after losing the tail race.
enum ConflictOutcome {
    /// This exact entry is already in the log at that sequence.
    AlreadyWritten(u64),
    /// Retry the append expecting this tail.
    Retry(u64),
}

/// Decide what a fenced append should do after losing the tail race, or refuse
/// to retry at all. `entries` is everything appended past the expected tail.
///
/// Only a `Cancel` is a reason not to retry, and only for the turn's own
/// writer: it ended the turn, so the entry is abandoned rather than written
/// behind the interruption. Everything else that can move the tail — input
/// queued behind the running turn, the control listener's HITL entries, a
/// sub-agent or handoff marker written through another handle on the same
/// lease — is a writer this worker appends in front of, not a rival.
fn resolve_conflict(
    entry: &harnx_core::session::SessionLogEntry,
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    rule: WriterRule,
) -> Result<ConflictOutcome> {
    use harnx_core::session::SessionLogEntry;
    let (tail, _) = entries
        .last()
        .context("session log rejected an append without a newer entry")?;
    for (seq, written) in entries {
        // Wind-up's whole output is this one entry, so finding the round
        // already answered is this same wind-up having got through: take that
        // sequence as our own rather than appending a duplicate.
        if matches!(rule, WriterRule::WindUp) && answers_the_same_calls(entry, written) {
            return Ok(ConflictOutcome::AlreadyWritten(*seq));
        }
        if matches!(rule, WriterRule::Turn) && matches!(written, SessionLogEntry::Cancel { .. }) {
            return Err(TurnInterrupted { cancel_seq: *seq }.into());
        }
    }
    Ok(ConflictOutcome::Retry(*tail))
}

/// Whether `written` already answers a call our wind-up entry is answering.
///
/// Two workers closing out the same interrupted round do NOT produce equal
/// entries: each stamps its own timestamp, and a reply that reached the
/// journal between them turns a placeholder into a real result. Whole-entry
/// equality therefore never recognised the other worker's `ToolResults`, and
/// the conflict fell through to a retry that appended a second answer for a
/// round the log had already closed. Answering any of the same calls is what
/// makes two entries the same wind-up.
fn answers_the_same_calls(
    entry: &harnx_core::session::SessionLogEntry,
    written: &harnx_core::session::SessionLogEntry,
) -> bool {
    use harnx_core::session::SessionLogEntry;
    let (
        SessionLogEntry::ToolResults { results: ours, .. },
        SessionLogEntry::ToolResults {
            results: theirs, ..
        },
    ) = (entry, written)
    else {
        return false;
    };
    ours.iter()
        .any(|ours| ours.id.is_some() && theirs.iter().any(|theirs| theirs.id == ours.id))
}

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

/// Worker sink bound to one lease owner.
///
/// Every append is stamped with the lease's revision and refused once the
/// lease is gone, so a fenced-out worker cannot write behind its replacement.
///
/// HITL also retains the expected stream tail used to derive the decision. A
/// conditional append that loses forces re-derivation. The continuation
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
        block_on_io(async move {
            sink.append_hitl_event_cas(&entry, expected_last_sequence)
                .await
        })
    }

    fn fenced_entry(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> harnx_core::session::SessionLogEntry {
        stamp_fence_token(entry, self.lease.fence_token())
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
        let fenced = self.fenced_entry(entry);
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
        self.ensure_lease_held(lease, entry)?;
        let seq = match log
            .append_fenced(
                entry,
                expected_last_sequence,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await?
        {
            crate::nats_session_log::FencedAppend::Appended(seq) => Some(seq),
            crate::nats_session_log::FencedAppend::Conflict { .. } => None,
        };
        if let Some(seq) = seq {
            self.observe_append(seq);
        }
        Ok(seq)
    }

    /// Append one entry at `expected_tail` under the turn writer's rule: a
    /// newer `Cancel` ends the turn, so the entry is abandoned with a
    /// [`TurnInterrupted`] error rather than written behind the interruption.
    pub(crate) async fn append_event_fenced_with_lease(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        lease: &NatsSessionLease,
        expected_tail: u64,
    ) -> Result<u64> {
        self.append_under_writer_rule(
            entry,
            lease,
            FencedWrite {
                expected_tail,
                rule: WriterRule::Turn,
                message_id: uuid::Uuid::new_v4().to_string(),
            },
        )
        .await
    }

    /// Append the wind-up of an interrupted turn. Unlike a turn's own writer
    /// it keeps going past another `Cancel`: the log still owes the
    /// interrupted calls a result.
    pub(crate) async fn append_wind_up_fenced_with_lease(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        lease: &NatsSessionLease,
        round: WoundUpRound,
    ) -> Result<u64> {
        self.append_under_writer_rule(
            entry,
            lease,
            FencedWrite {
                expected_tail: round.expected_tail,
                rule: WriterRule::WindUp,
                message_id: round.message_id(&self.session_id),
            },
        )
        .await
    }

    async fn append_under_writer_rule(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        lease: &NatsSessionLease,
        mut write: FencedWrite,
    ) -> Result<u64> {
        let entry = stamp_fence_token(entry, lease.fence_token());
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        // One message id for every attempt: a publish rejected for the wrong
        // tail never enters the dedupe window, while one whose ack was lost
        // is recognised as itself instead of appended a second time.
        for _ in 0..FENCED_APPEND_ATTEMPTS {
            // Every round is a fresh write decision made against a tail this
            // worker has just re-read, and the lease can have gone in the
            // meantime — checking only on the way in would let a worker that
            // was fenced out mid-retry append under a token it no longer holds.
            self.ensure_lease_held(lease, &entry)?;
            match log
                .append_fenced(&entry, write.expected_tail, &write.message_id)
                .await?
            {
                crate::nats_session_log::FencedAppend::Appended(seq) => {
                    self.observe_append(seq);
                    return Ok(seq);
                }
                crate::nats_session_log::FencedAppend::Conflict { entries } => {
                    match resolve_conflict(&entry, &entries, write.rule)? {
                        ConflictOutcome::AlreadyWritten(seq) => {
                            self.observe_append(seq);
                            return Ok(seq);
                        }
                        ConflictOutcome::Retry(tail) => write.expected_tail = tail,
                    }
                }
            }
        }
        anyhow::bail!(
            "session log tail kept moving while appending: session_id={} entry_type={}",
            self.session_id,
            crate::session_history::entry_type(&entry)
        )
    }

    fn ensure_lease_held(
        &self,
        lease: &NatsSessionLease,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> Result<()> {
        if lease.is_held() {
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

    /// A lease-holding writer appends under the turn writer's rule, so a
    /// `Cancel` that ended the turn stops the entry instead of letting it land
    /// behind the interruption. The expected tail comes from the shared
    /// `after_seq_observer` when this worker has already written something this
    /// activation, and otherwise from a tail read.
    async fn append_event_with_lease(
        &self,
        entry: &harnx_core::session::SessionLogEntry,
        lease: Option<&NatsSessionLease>,
    ) -> Result<u64> {
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        let Some(lease) = lease else {
            return self.append_unfenced_entry(&log, entry).await;
        };
        self.ensure_lease_held(lease, entry)?;
        let expected_tail = match self.observed_append_seq() {
            Some(seq) => seq,
            // No observer: this backend has no memory of where the turn last
            // left the log, so the current tail is the best expectation it can
            // form. A `Cancel` that lands after this read still stops the
            // append, through the retry that reads the conflict.
            None => log.last_entry_async().await?.map_or(0, |(seq, _)| seq),
        };
        self.append_event_fenced_with_lease(entry, lease, expected_tail)
            .await
    }

    /// Append without a lease: no turn owns this session in this process, so
    /// there is no tail to fence against. Used by direct `run_agent_loop`
    /// callers that persist through the backend rather than a worker lease.
    async fn append_unfenced_entry(
        &self,
        log: &crate::nats_session_log::NatsSessionLog,
        entry: &harnx_core::session::SessionLogEntry,
    ) -> Result<u64> {
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
        block_on_io(async move {
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

    /// The tail this worker last observed: seeded at activation from the
    /// stream and advanced by every append it makes. `None` when no observer
    /// is attached, which is not the same as an observer reading zero.
    fn observed_append_seq(&self) -> Option<u64> {
        self.after_seq_observer
            .as_ref()
            .map(|observer| observer.load(std::sync::atomic::Ordering::Relaxed))
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

    pub async fn last_entry_async(
        &self,
    ) -> Result<Option<(u64, harnx_core::session::SessionLogEntry)>> {
        let log = crate::nats_session_log::NatsSessionLog::new(
            self.jetstream.clone(),
            self.session_id.clone(),
        );
        log.last_entry_async().await
    }
}

#[cfg(test)]
pub(crate) async fn test_session_authority(
    jetstream: &jetstream::Context,
    session_id: &str,
    store: &crate::nats_session_metadata::SessionMetadataStore,
) -> Arc<NatsSessionLease> {
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
    Arc::new(lease)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::session::{SessionLogEntry, ToolOutput};

    fn results(ids: &[&str], stamp: i64) -> SessionLogEntry {
        SessionLogEntry::ToolResults {
            results: ids
                .iter()
                .map(|id| ToolOutput {
                    id: Some((*id).to_string()),
                    name: "probe".into(),
                    output: serde_json::json!({"stamp": stamp}),
                    markdown: None,
                    content: Vec::new(),
                    switch_agent: None,
                })
                .collect(),
            timestamp: chrono::DateTime::from_timestamp(stamp, 0),
        }
    }

    /// Two workers closing out the same interrupted round never write the
    /// same bytes: each stamps its own timestamp, and a reply that reached the
    /// journal in between turns a placeholder into a real result. The round is
    /// still answered, so the second one adopts that sequence instead of
    /// appending a second answer behind it.
    #[test]
    fn a_wind_up_recognises_another_workers_answer_to_its_round() {
        let outcome = resolve_conflict(
            &results(&["c1", "c2"], 200),
            &[(9, results(&["c1", "c2"], 100))],
            WriterRule::WindUp,
        )
        .expect("a wind-up never refuses to resolve a conflict");
        assert!(
            matches!(outcome, ConflictOutcome::AlreadyWritten(9)),
            "the round is answered at sequence 9"
        );
    }

    /// Results for some other round are ordinary tail movement: the wind-up
    /// still owes its own calls an answer and appends in front of them.
    #[test]
    fn a_wind_up_appends_in_front_of_results_for_another_round() {
        let outcome = resolve_conflict(
            &results(&["c1"], 200),
            &[(9, results(&["other-round-call"], 100))],
            WriterRule::WindUp,
        )
        .expect("a wind-up never refuses to resolve a conflict");
        assert!(
            matches!(outcome, ConflictOutcome::Retry(9)),
            "nothing here answers this wind-up's calls"
        );
    }
}
