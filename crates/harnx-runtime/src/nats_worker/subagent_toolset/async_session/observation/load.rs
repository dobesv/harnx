//! Pure prompt-scope resolution for session_load (plan #1550 Task 4b).
//!
//! Prompt-scoped load validates the target prompt binding against its durable
//! admission and resolves the bounds of that prompt's turn so later unrelated
//! turns stay excluded. It reuses the observer's bounded lifecycle scan with
//! none of its side effects: no repair, activation, resume or polling.

use super::{
    super::SessionPromptStatus,
    budget::{Budget, Stop},
    scope::Scope,
    snapshot::Snapshot,
    SubagentToolset,
};
use crate::nats_session_log::NatsSessionLog;
use tokio_util::sync::CancellationToken;

/// Durable bounds of one prompt's turn: the original prompt's sequence and
/// the end of its turn — the first applicable durable terminal, or the scan
/// position when no terminal has been reached yet.
pub(in crate::nats_worker::subagent_toolset) struct PromptScope {
    pub(in crate::nats_worker::subagent_toolset) start: u64,
    pub(in crate::nats_worker::subagent_toolset) end: u64,
    pub(in crate::nats_worker::subagent_toolset) status: SessionPromptStatus,
}

pub(in crate::nats_worker::subagent_toolset) enum PromptScopeError {
    /// No admission binds this prompt in this agent's namespace.
    UnknownPrompt,
    /// The durable history cannot prove the original prompt (content
    /// mismatch, gap, or not found). A pure read fails closed.
    Evidence(&'static str),
    /// Broker reads did not complete under the nonblocking I/O bound.
    Unavailable,
}

/// Resolve one prompt's durable turn bounds without any observer side
/// effect. session_load keeps its existing access namespace: the caller's
/// authority is not re-checked here, only the target binding.
pub(in crate::nats_worker::subagent_toolset) async fn resolve_prompt_scope(
    toolset: &SubagentToolset,
    storage: &str,
    prompt_id: &str,
) -> Result<PromptScope, PromptScopeError> {
    let admission = toolset
        .session_metadata
        .prompt_admission(storage, prompt_id)
        .await
        .map_err(|_| PromptScopeError::Unavailable)?
        .ok_or(PromptScopeError::UnknownPrompt)?;
    // A joined-dispatch record's content is authoritative for the prompt
    // identity check; without it the invocation's admission supplies the
    // content as before (issue #2672).
    let joined = toolset
        .session_metadata
        .joined_dispatch(storage, prompt_id)
        .await
        .map_err(|_| PromptScopeError::Unavailable)?;
    let log = NatsSessionLog::new(toolset.jetstream.clone(), storage);
    // Snapshot budget: one bounded pass under the shared 10 s I/O cap. Load must
    // never wait on the child, repair an activation or poll for progress.
    let budget = Budget::snapshot(CancellationToken::new());
    let scope = Scope {
        storage: storage.to_owned(),
        admission,
        joined,
        prompt_id: prompt_id.to_owned(),
        call_id: String::new(),
    };
    let mut snapshot = Snapshot::default();
    snapshot
        .refresh(&log, &scope, &budget)
        .await
        .map_err(scope_stop)?;
    if let Some(evidence) = snapshot.evidence_error() {
        return Err(PromptScopeError::Evidence(evidence));
    }
    let start = snapshot
        .prompt
        .ok_or(PromptScopeError::Evidence("original_prompt_not_found"))?;
    Ok(PromptScope {
        start,
        end: snapshot.output_end(),
        status: snapshot.status(),
    })
}

fn scope_stop(stop: Stop) -> PromptScopeError {
    // A snapshot budget allows one full pass; any stop means the shared I/O
    // bound was exhausted mid-scan, never that the prompt was disproven.
    match stop {
        Stop::Expired
        | Stop::Cancelled
        | Stop::Unavailable
        | Stop::NewMessage
        | Stop::CallerDeadline => PromptScopeError::Unavailable,
    }
}
