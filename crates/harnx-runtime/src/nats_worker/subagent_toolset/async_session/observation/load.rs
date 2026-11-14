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
use crate::nats_session_metadata::SessionMetadataStore;
use harnx_core::api_types::CompletionTokenUsage;
use std::time::Duration;
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

/// The dispatched-prompt progress watcher's durable scan. Unlike the observer
/// this handle has no deadline and no side effects; the snapshot keeps its
/// cursor across refreshes, so each poll reads only the durable entries that
/// landed since the previous one — a large child log is read once, not
/// rescanned every tick. Each `refresh` pass gets its own bounded `Budget`:
/// a snapshot budget covers exactly one logical pass, so a budget created at
/// bind would starve every refresh after its absolute deadline.
pub(in crate::nats_worker::subagent_toolset) struct WatchScan {
    scope: Scope,
    snapshot: Snapshot,
    /// Per-pass I/O bound; the observation default (`IO_LIMIT`) unless a test
    /// pinned a smaller one.
    pass_limit: Duration,
    log: NatsSessionLog,
    /// Test seam: the first `n` refreshes of THIS scan fail with a transient
    /// `Stop::Unavailable` before touching the durable log — the one broker
    /// failure that cannot be forced deterministically against a live test
    /// server. Zero in production.
    #[cfg(test)]
    forced_refresh_failures: std::cell::Cell<usize>,
}

/// Test seam quota (R6-B1): how many refresh failures the NEXT bound watch
/// scan is forced to report. A test sets it before starting the watcher
/// whose failure window it needs; bind consumes it wholesale, so scans bound
/// before or after are unaffected. `nextest` runs each test in its own
/// process, so the quota never leaks between tests.
#[cfg(test)]
pub(in crate::nats_worker::subagent_toolset) fn set_forced_refresh_failures(failures: usize) {
    forced_refresh_failure_quota().store(failures, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
fn take_forced_refresh_failures() -> usize {
    forced_refresh_failure_quota().swap(0, std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
fn forced_refresh_failure_quota() -> &'static std::sync::atomic::AtomicUsize {
    static QUOTA: std::sync::OnceLock<std::sync::atomic::AtomicUsize> = std::sync::OnceLock::new();
    QUOTA.get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
}

pub(in crate::nats_worker::subagent_toolset) enum WatchBindError {
    /// The prompt binding is gone; nothing durable remains to follow.
    UnknownPrompt,
    /// Broker reads did not complete; the caller can retry later.
    Unavailable,
}

pub(in crate::nats_worker::subagent_toolset) enum WatchState {
    /// Still running, or blocked on an approval.
    Pending,
    /// Durable settlement, with the `TurnEnd` usage total when the terminal
    /// row is a `TurnEnd` that carries one.
    Terminal {
        status: SessionPromptStatus,
        usage: Option<CompletionTokenUsage>,
    },
    /// The bounded scan cannot prove the prompt's history any more
    /// (compaction gap, content mismatch); nothing later repairs that.
    EvidenceError(&'static str),
}

/// The one prompt a watcher binds: its durable stores, storage key, prompt
/// identity and per-pass scan bound. A struct so `WatchScan::bind` stays at
/// one argument instead of five.
pub(in crate::nats_worker::subagent_toolset) struct WatchTarget<'a> {
    pub(in crate::nats_worker::subagent_toolset) metadata: &'a SessionMetadataStore,
    pub(in crate::nats_worker::subagent_toolset) jetstream: &'a async_nats::jetstream::Context,
    pub(in crate::nats_worker::subagent_toolset) storage: &'a str,
    pub(in crate::nats_worker::subagent_toolset) prompt_id: &'a str,
    pub(in crate::nats_worker::subagent_toolset) pass_limit: Duration,
}

impl WatchScan {
    pub(in crate::nats_worker::subagent_toolset) async fn bind(
        target: WatchTarget<'_>,
    ) -> Result<Self, WatchBindError> {
        let WatchTarget {
            metadata,
            jetstream,
            storage,
            prompt_id,
            pass_limit,
        } = target;
        let admission = metadata
            .prompt_admission(storage, prompt_id)
            .await
            .map_err(|_| WatchBindError::Unavailable)?
            .ok_or(WatchBindError::UnknownPrompt)?;
        // A prompt that joined a busy turn carries its original content in the
        // joined-dispatch record (#2672); without it the scan would flag the
        // prompt as a content mismatch. Same binding check as `Scope::resolve`.
        let joined = metadata
            .joined_dispatch(storage, prompt_id)
            .await
            .map_err(|_| WatchBindError::Unavailable)?;
        if joined
            .as_ref()
            .is_some_and(|joined| joined.invocation_id != admission.invocation_id.as_str())
        {
            return Err(WatchBindError::UnknownPrompt);
        }
        Ok(Self {
            scope: Scope {
                storage: storage.to_owned(),
                admission,
                joined,
                prompt_id: prompt_id.to_owned(),
                call_id: String::new(),
            },
            snapshot: Snapshot::default(),
            pass_limit,
            log: NatsSessionLog::new(jetstream.clone(), storage),
            #[cfg(test)]
            forced_refresh_failures: std::cell::Cell::new(take_forced_refresh_failures()),
        })
    }

    /// One bounded pass over the entries that landed since the previous one.
    /// A fresh `Budget` per pass: a snapshot budget covers exactly one
    /// logical pass under its absolute deadline, so reusing one created at
    /// bind would fail every refresh past the pass limit and leave the
    /// watcher blind (a child running longer than that would never get its
    /// terminal).
    ///
    /// `Err` carries the pass's `Stop`: a transient broker failure the caller
    /// should log and retry on its next tick, never a settlement. The cursor
    /// stays in the snapshot, so the next pass resumes where this one stopped.
    pub(in crate::nats_worker::subagent_toolset) async fn refresh(
        &mut self,
    ) -> Result<WatchState, Stop> {
        // Test seam first: a forced failure must not advance the snapshot's
        // cursor — the next real pass resumes where the last real one
        // stopped, exactly like a broker read that never happened.
        #[cfg(test)]
        if self.forced_refresh_failures.get() > 0 {
            self.forced_refresh_failures
                .set(self.forced_refresh_failures.get() - 1);
            return Err(Stop::Unavailable);
        }
        let budget = Budget::snapshot_with_pass_limit(CancellationToken::new(), self.pass_limit);
        self.snapshot
            .refresh(&self.log, &self.scope, &budget)
            .await?;
        if let Some((status, usage)) = self.snapshot.terminal_evidence() {
            return Ok(WatchState::Terminal { status, usage });
        }
        if let Some(evidence) = self.snapshot.evidence_error() {
            return Ok(WatchState::EvidenceError(evidence));
        }
        Ok(WatchState::Pending)
    }
}
