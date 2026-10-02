//! Runtime persistence adapter for in-memory session state.

use super::GlobalConfig;
use crate::nats_session_metadata::{SessionOverrideUpdate, SessionOverrides};
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

    /// Recheck exact sink authority when a legacy bool-returning append lost its error.
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
    match append_through(sink(session), session.id(), entry) {
        Some(seq) => {
            session.log_entry_count = seq as usize;
            true
        }
        None => false,
    }
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
) -> Option<u64> {
    let Some(append_sink) = sink else {
        log::warn!(
            "session append dropped: no persistence sink attached (session_id={session_id} entry_type={})",
            crate::session_history::entry_type(entry)
        );
        return None;
    };
    match append_sink.append(entry) {
        Ok(seq) => Some(seq),
        Err(error) => {
            log::warn!(
                "session append failed: session_id={session_id} entry_type={} error={error}",
                crate::session_history::entry_type(entry)
            );
            None
        }
    }
}

pub(super) fn require_authoritative_appends(
    session: &Session,
    all_appended: bool,
    operation: &str,
) -> Result<()> {
    if all_appended {
        return Ok(());
    }
    if let Some(sink) = sink(session).filter(|sink| sink.failure_is_fatal()) {
        sink.validate_output()?;
        anyhow::bail!("failed to durably persist {operation}");
    }
    Ok(())
}

/// The active session's persistence sink, if it is `session_id`.
///
/// Callers take this under a short guard and drop the guard before they
/// persist anything: the sink's methods are NATS round trips, and the config
/// lock must never be held across one.
fn active_session_sink(
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

/// A title and how it was chosen, as [`record_title`] persists it.
pub struct TitleRecord {
    pub title: String,
    /// Set by the user. A manual title stops automatic regeneration.
    pub manual: bool,
    /// The session's token count when the title was chosen.
    pub tokens: usize,
}

/// Persist canonical title metadata for session `session_id`, then set the
/// in-memory title if that session is still the active one. Returns whether
/// the in-memory title was set.
///
/// Call it without holding a config guard. It takes one only to find the
/// sink and again to set the title, and writes the metadata in between with
/// neither held. Two title writes to one config never overlap here:
/// background titles run only where the agent loop runs, a worker's
/// per-session config, and are single-flight through the session's
/// `titling` flag, while front-end commands (`.set title`, `.title
/// generate`) run one at a time on the front end's own config.
pub fn record_title(config: &GlobalConfig, session_id: &str, record: TitleRecord) -> Result<bool> {
    let TitleRecord {
        title,
        manual,
        tokens,
    } = record;
    if let Some(sink) = active_session_sink(config, Some(session_id)) {
        sink.persist_title(&title, manual, tokens)
            .context("failed to durably persist session title")?;
    }
    let mut guard = config.write();
    let Some(session) = guard.session.as_mut().filter(|s| s.id == session_id) else {
        return Ok(false);
    };
    session.set_title(title);
    session.set_title_last_updated_tokens(if manual { usize::MAX } else { tokens });
    Ok(true)
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
