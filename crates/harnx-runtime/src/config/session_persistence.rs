//! Runtime persistence adapter for in-memory session state.

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
    if let Some(append_sink) = sink(session) {
        return match append_sink.append(entry) {
            Ok(seq) => {
                session.log_entry_count = seq as usize;
                true
            }
            Err(error) => {
                log::warn!(
                    "session append failed: session_id={} entry_type={} error={error}",
                    session.id(),
                    crate::session_history::entry_type(entry)
                );
                false
            }
        };
    }

    log::warn!(
        "session append dropped: no persistence sink attached (session_id={} entry_type={})",
        session.id(),
        crate::session_history::entry_type(entry)
    );
    false
}

pub(super) fn require_authoritative_appends(
    session: &Session,
    all_appended: bool,
    operation: &str,
) -> Result<()> {
    if all_appended {
        return Ok(());
    }
    if sink(session).is_some_and(|sink| sink.failure_is_fatal()) {
        anyhow::bail!("failed to durably persist {operation}");
    }
    Ok(())
}

/// Persist canonical title metadata before updating the in-memory title.
pub fn record_title(
    session: &mut Session,
    title: String,
    manual: bool,
    tokens: usize,
) -> Result<()> {
    if let Some(sink) = sink(session) {
        sink.persist_title(&title, manual, tokens)
            .context("failed to durably persist session title")?;
    }
    session.set_title(title);
    session.set_title_last_updated_tokens(if manual { usize::MAX } else { tokens });
    Ok(())
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

pub fn persist_session_override(session: &Session, update: &SessionOverrideUpdate) -> Result<()> {
    if let Some(sink) = sink(session) {
        sink.persist_override(update)?;
    }
    Ok(())
}
