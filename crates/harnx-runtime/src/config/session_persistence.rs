//! Runtime persistence adapter for in-memory session state.

mod append_status;
pub(crate) use append_status::AppendStatus;

use super::{Config, GlobalConfig};
use crate::nats_session_metadata::{
    SessionMetadata, SessionOverrideUpdate, SessionOverrides, SessionPropertiesUpdate,
};
use anyhow::{Context, Result};
use harnx_core::agent_config::AgentVariables;
use harnx_core::execution_context::ExecutionContextObservation;
use harnx_core::session::{Session, SessionLogEntry};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub trait SessionAppendSink: Send + Sync + Any {
    /// Append an entry and return its one-based durable sequence number.
    fn append(&self, entry: &SessionLogEntry) -> Result<u64>;

    /// Durable middleware support is available only from a lease-owning NATS sink.
    fn interaction_session(
        &self,
    ) -> Option<crate::interaction_checkpoint::live::InteractionSession> {
        None
    }

    /// Recheck exact sink authority before reporting a failed output append.
    fn validate_output(&self) -> Result<()> {
        Ok(())
    }

    /// Whether an append failure makes the active turn invalid. File-backed
    /// sessions can mark themselves dirty and rewrite later; a NATS worker log
    /// is authoritative and must never publish a successful turn boundary
    /// after losing an assistant/tool entry.
    fn failure_is_fatal(&self) -> bool {
        false
    }

    /// Persist title state outside the transcript before the in-memory session
    /// changes. Non-NATS/test sinks may leave it in memory only.
    fn persist_title(&self, _title: &str, _manual: bool, _tokens: usize) -> Result<()> {
        Ok(())
    }

    /// Persist the complete explicit override set before applying a runtime
    /// setting change in memory.
    fn persist_overrides(&self, _overrides: &SessionOverrides) -> Result<()> {
        Ok(())
    }

    /// Persist one explicit override field before applying it in memory.
    fn persist_override(&self, _update: &SessionOverrideUpdate) -> Result<()> {
        Ok(())
    }

    fn load_overrides(&self) -> Result<Option<SessionOverrides>> {
        Ok(None)
    }

    fn persist_variables(&self, _variables: &AgentVariables) -> Result<()> {
        Ok(())
    }

    fn persist_execution_contexts<'a>(
        &'a self,
        _observations: &'a [ExecutionContextObservation],
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    /// The session's canonical metadata, or `None` from a sink that keeps none.
    fn load_metadata(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<SessionMetadata>>> + Send + '_>> {
        Box::pin(async { Ok(None) })
    }

    /// Apply an agent's change to the session's properties and return the
    /// metadata it committed, or `None` from a sink that keeps no metadata.
    fn persist_session_properties<'a>(
        &'a self,
        _update: &'a SessionPropertiesUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<Option<SessionMetadata>>> + Send + 'a>> {
        Box::pin(async { Ok(None) })
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemorySessionLogSink {
    entries: std::sync::Mutex<Vec<SessionLogEntry>>,
}

#[cfg(test)]
impl SessionAppendSink for MemorySessionLogSink {
    fn append(&self, entry: &SessionLogEntry) -> Result<u64> {
        let mut entries = self.entries.lock().expect("memory session log poisoned");
        entries.push(entry.clone());
        Ok(entries.len() as u64)
    }
}

#[cfg(test)]
pub(crate) fn attach_memory_log(session: &mut Session) {
    session.runtime = Some(Arc::new(
        Arc::new(MemorySessionLogSink::default()) as Arc<dyn SessionAppendSink>
    ));
}

fn sink(session: &Session) -> Option<&Arc<dyn SessionAppendSink>> {
    session
        .runtime
        .as_ref()?
        .downcast_ref::<Arc<dyn SessionAppendSink>>()
}

#[must_use = "execution-context persistence must be awaited"]
pub(crate) struct PendingExecutionContextPersistence {
    sink: Option<Arc<dyn SessionAppendSink>>,
    session_id: String,
    observations: Vec<ExecutionContextObservation>,
}

impl PendingExecutionContextPersistence {
    pub(crate) fn none(session_id: impl Into<String>) -> Self {
        Self {
            sink: None,
            session_id: session_id.into(),
            observations: Vec::new(),
        }
    }

    pub(crate) fn for_session(
        session: &Session,
        observations: Vec<ExecutionContextObservation>,
        tool_results_are_durable: bool,
    ) -> Self {
        Self {
            sink: tool_results_are_durable
                .then(|| sink(session).cloned())
                .flatten(),
            session_id: session.id().to_string(),
            observations,
        }
    }

    pub(crate) async fn persist(self) {
        if self.observations.is_empty() {
            return;
        }
        let Some(sink) = self.sink else {
            return;
        };
        if let Err(error) = sink.persist_execution_contexts(&self.observations).await {
            log::warn!(
                "failed to persist tool-observed execution context: session_id={} error={error:#}",
                self.session_id
            );
        }
    }
}

/// Append a log entry through the session's runtime persistence sink.
pub fn append_event(session: &mut Session, entry: &SessionLogEntry) -> bool {
    append_event_status(session, entry).is_complete()
}

/// Preserve the cause until the caller decides whether its sink is authoritative.
pub(crate) fn append_event_status(session: &mut Session, entry: &SessionLogEntry) -> AppendStatus {
    AppendStatus::from_result(append_event_seq(session, entry))
}

/// [`append_event`], retaining failures and the entry's durable sequence.
pub(crate) fn append_event_seq(
    session: &mut Session,
    entry: &SessionLogEntry,
) -> Result<Option<u64>> {
    let seq = append_through(sink(session), session.id(), entry)?;
    if let Some(seq) = seq {
        session.log_entry_count = seq as usize;
    }
    Ok(seq)
}

/// The session's persistence sink, cloned so a caller can append through it
/// after releasing the config guard the session was borrowed from.
pub(super) fn detached_sink(session: &Session) -> Option<Arc<dyn SessionAppendSink>> {
    sink(session).cloned()
}

/// Append through a sink taken from a session, returning the entry's durable
/// sequence. Failures are logged here, as `append_event` does.
pub(super) fn append_through(
    sink: Option<&Arc<dyn SessionAppendSink>>,
    session_id: &str,
    entry: &SessionLogEntry,
) -> Result<Option<u64>> {
    let Some(append_sink) = sink else {
        log::warn!(
            "session append dropped: no persistence sink attached (session_id={session_id} entry_type={})",
            crate::session_history::entry_type(entry)
        );
        return Ok(None);
    };
    match append_sink.append(entry) {
        Ok(seq) => Ok(Some(seq)),
        Err(error) => {
            log::warn!(
                "session append failed: session_id={session_id} entry_type={} error={error}",
                crate::session_history::entry_type(entry)
            );
            Err(error)
        }
    }
}

pub(super) fn require_authoritative_appends(
    session: &Session,
    appends: AppendStatus,
    operation: &str,
) -> Result<()> {
    if appends.is_complete() {
        return Ok(());
    }
    if let Some(sink) = sink(session).filter(|sink| sink.failure_is_fatal()) {
        sink.validate_output()?;
        return appends
            .into_result()
            .with_context(|| format!("failed to durably persist {operation}"));
    }
    Ok(())
}

/// The active session's persistence sink, if it is `session_id`.
///
/// Callers take this under a short guard and drop the guard before they
/// persist anything: the sink's methods are NATS round trips, and the config
/// lock must never be held across one.
pub(crate) fn active_session_sink(
    config: &GlobalConfig,
    session_id: Option<&str>,
) -> Option<Arc<dyn SessionAppendSink>> {
    let guard = config.read();
    let session = guard.session.as_ref()?;
    if session_id.is_some_and(|id| session.id != id) {
        return None;
    }
    detached_sink(session)
}

/// The active session's canonical storage key and persistence sink, if one
/// is currently installed.
pub fn active_session_sink_with_storage_key(
    config: &GlobalConfig,
) -> Option<(String, Arc<dyn SessionAppendSink>)> {
    let guard = config.read();
    let session = guard.session.as_ref()?;
    let sink = detached_sink(session)?;
    Some((session.storage_key(), sink))
}

/// Install `sink` onto the active session when it is still sinkless and
/// matches `expected_storage_key`. Never overwrites an existing handle.
// qual:allow(iosp) reason: "writes sink only after checking storage-key match and sinkless precondition under one write guard"
pub fn install_session_sink_if_missing(
    config: &GlobalConfig,
    expected_storage_key: &str,
    sink: Arc<dyn SessionAppendSink>,
) -> bool {
    let mut guard = config.write();
    let Some(session) = guard.session.as_mut() else {
        return false;
    };
    if session.storage_key() != expected_storage_key || session.runtime.is_some() {
        return false;
    }
    session.runtime = Some(Arc::new(sink));
    true
}

/// A title and how it was chosen, as [`record_title`] persists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TitleRecord {
    pub title: String,
    /// Set by the user. A manual title stops automatic regeneration.
    pub manual: bool,
    /// The session's token count when the title was chosen.
    pub tokens: usize,
}

/// A title write's full destination, captured under one config read so a
/// concurrent `.session` switch can never mix record and destination state:
/// the session's id and storage key, the cluster its broker connection
/// resolves through, a routing snapshot for the broker acquisition, and
/// the exact persistence sink if one is installed. The short session id
/// alone is not an identity — different agents or clusters can reuse it
/// while targeting different storage keys.
#[derive(Clone)]
pub struct TitleWriteTarget {
    session_id: String,
    storage_key: String,
    cluster: String,
    routing: Config,
    sink: Option<Arc<dyn SessionAppendSink>>,
}

impl TitleWriteTarget {
    /// Whether the captured sink exists. The sink-free case is the
    /// pre-first-turn session; a durable write then has to obtain its own
    /// broker connection.
    pub(crate) fn has_sink(&self) -> bool {
        self.sink.is_some()
    }

    pub fn matches_config(&self, config: &Config) -> bool {
        title_target_still_active(config, self)
    }
}

/// The cluster a session's broker traffic resolves through: the remote
/// agent's cluster when one is selected, the default cluster otherwise.
fn active_cluster(config: &Config) -> &str {
    config
        .remote_agent
        .as_ref()
        .map(|(_, cluster)| cluster.as_str())
        .unwrap_or_else(|| config.default_cluster_key())
}

/// Whether `target` is still the active session, by full identity rather
/// than short id: id, storage key, cluster, and active agent ref must all
/// match. A same-id switch to a different agent or cluster fails this fence,
/// so a stale completion can neither retitle another session's memory nor
/// report success for it.
fn title_target_still_active(config: &Config, target: &TitleWriteTarget) -> bool {
    config.session.as_ref().is_some_and(|session| {
        session.id == target.session_id
            && session.storage_key() == target.storage_key
            && active_cluster(config) == target.cluster
            && config.active_agent_ref() == target.routing.active_agent_ref()
    })
}

fn build_title_target(config: &Config, session: &Session) -> TitleWriteTarget {
    TitleWriteTarget {
        session_id: session.id.clone(),
        storage_key: session.storage_key(),
        cluster: active_cluster(config).to_string(),
        routing: config.clone(),
        sink: detached_sink(session),
    }
}

/// Capture the title destination and active session token count atomically
/// under one config read. Returns `None` when no session is active.
pub(crate) fn capture_active_title_target(
    config: &GlobalConfig,
) -> Option<(TitleWriteTarget, usize)> {
    let guard = config.read();
    let session = guard.session.as_ref()?;
    Some((build_title_target(&guard, session), session.tokens))
}

/// Capture the title destination for `session_id` atomically: one guard
/// covers identity, routing, and sink, so the probe and the write can never
/// observe different sessions. Returns `None` when `session_id` is not the
/// active session.
pub fn capture_title_target(config: &GlobalConfig, session_id: &str) -> Option<TitleWriteTarget> {
    let guard = config.read();
    let session = guard
        .session
        .as_ref()
        .filter(|session| session.id == session_id)?;
    Some(build_title_target(&guard, session))
}

/// Persist a title through an already-captured destination, then apply the
/// fenced in-memory update. The captured sink is used exactly once; a
/// session replaced between capture and completion never receives the
/// write, the memory update, or a success report.
pub fn record_title_captured(
    config: &GlobalConfig,
    target: TitleWriteTarget,
    record: TitleRecord,
) -> Result<bool> {
    let TitleRecord {
        title,
        manual,
        tokens,
    } = record;
    if let Some(sink) = &target.sink {
        sink.persist_title(&title, manual, tokens)
            .context("failed to durably persist session title")?;
    }
    Ok(apply_recorded_title(config, &target, title, manual, tokens))
}

/// Persist canonical title metadata for session `session_id`, then set the
/// in-memory title if that session is still the active one. Returns whether
/// the in-memory title was set.
///
/// Call it without holding a config guard. It captures the destination
/// under one short read and drops the guard before persisting; the sink's
/// methods are NATS round trips, and the config lock must never be held
/// across one. Two title writes to one config never overlap here:
/// background titles run only where the agent loop runs, a worker's
/// per-session config, and are single-flight through the session's
/// `titling` flag, while front-end commands (`.set title`, `.title
/// generate`) run one at a time on the front end's own config.
pub fn record_title(config: &GlobalConfig, session_id: &str, record: TitleRecord) -> Result<bool> {
    let Some(target) = capture_title_target(config, session_id) else {
        return Ok(false);
    };
    record_title_captured(config, target, record)
}

/// Durable manual-title write against an already-captured destination
/// (`TitleWriteTarget`), covering both installed-sink and pre-first-turn
/// (sink-less) sessions. Rejects stale targets before any persistence or
/// broker connection, never recaptures live selection, and fences the
/// in-memory update on `target` still being active.
pub async fn record_title_durably(
    config: &GlobalConfig,
    target: TitleWriteTarget,
    record: TitleRecord,
    abort: Option<&crate::utils::AbortSignal>,
) -> Result<bool> {
    if abort.is_some_and(|signal| signal.aborted()) {
        anyhow::bail!("session title write cancelled");
    }
    if !target.matches_config(&config.read()) {
        return Ok(false);
    }
    if let Some(sink) = &target.sink {
        // The sink captured before any await: a session switch during the
        // round trip cannot redirect the write to another session's log.
        sink.persist_title(&record.title, record.manual, record.tokens)
            .context("failed to durably persist session title")?;
    } else {
        let persist = persist_title_without_sink(
            &target.routing,
            &target.cluster,
            &target.storage_key,
            &record,
            abort,
        );
        // Remote endpoint connect is not abort-aware; race the whole explicit
        // write against the caller's signal like the read-only history loader.
        match abort {
            Some(abort) => {
                tokio::select! {
                    biased;
                    _ = harnx_core::abort::wait_abort_signal(abort) =>
                        anyhow::bail!("session title write cancelled"),
                    result = persist => result?,
                }
            }
            None => persist.await?,
        }
    }
    Ok(apply_recorded_title(
        config,
        &target,
        record.title,
        record.manual,
        record.tokens,
    ))
}

async fn persist_title_without_sink(
    routing: &Config,
    cluster: &str,
    storage_key: &str,
    record: &TitleRecord,
    abort: Option<&crate::utils::AbortSignal>,
) -> Result<()> {
    let (server, client) = routing.resolve_nats_connection(cluster, abort).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = crate::nats_session_metadata::SessionMetadataStore::ensure(
        &jetstream,
        server.resolved_replicas(),
    )
    .await
    .context("failed to open canonical session metadata store")?;
    store
        .patch(storage_key, |metadata| {
            metadata.title.value = Some(record.title.clone());
            metadata.title.manual = record.manual;
            metadata.title.last_updated_tokens = record.tokens;
            Ok(())
        })
        .await
        .context("failed to durably persist session title")?;
    Ok(())
}

/// Fenced in-memory half of a title write: the title lands only when the
/// active session still matches the captured full target (id, storage key,
/// cluster), so a session switch between the durable write and this step
/// never retitles the wrong session — including a replacement that reuses
/// the same short session id.
fn apply_recorded_title(
    config: &GlobalConfig,
    target: &TitleWriteTarget,
    title: String,
    manual: bool,
    tokens: usize,
) -> bool {
    let mut guard = config.write();
    if !title_target_still_active(&guard, target) {
        return false;
    }
    // `still_active` only passes with a session installed; the `None` arm is
    // that unreachable case, reported like a fence failure rather than a
    // panic.
    match guard.session.as_mut() {
        Some(session) => {
            session.set_title(title);
            session.set_title_last_updated_tokens(if manual { usize::MAX } else { tokens });
            true
        }
        None => false,
    }
}

pub fn session_overrides(session: &Session) -> Result<SessionOverrides> {
    if let Some(overrides) = sink(session)
        .map(|sink| sink.load_overrides())
        .transpose()?
        .flatten()
    {
        return Ok(overrides);
    }
    Ok(SessionOverrides {
        model: Some(session.model().id()),
        temperature: session.temperature(),
        top_p: session.top_p(),
        use_tools: session.use_tools(),
        bash_tools: session.bash_tools(),
        model_fallbacks: session.model_fallbacks.clone(),
        compress_threshold: session.compress_threshold,
        compaction_agent: session.compaction_agent.clone(),
        max_output_tokens: session.model().max_output_tokens(),
    })
}

pub fn persist_session_overrides(session: &Session, overrides: &SessionOverrides) -> Result<()> {
    if let Some(sink) = sink(session) {
        sink.persist_overrides(overrides)?;
    }
    Ok(())
}

/// Persist one explicit override field for the active session before the
/// caller applies it in memory. Call it without holding a config guard: it
/// takes one only to find the sink, and writes the override after dropping
/// it.
pub fn persist_active_session_override(
    config: &GlobalConfig,
    update: &SessionOverrideUpdate,
) -> Result<()> {
    if let Some(sink) = active_session_sink(config, None) {
        sink.persist_override(update)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
