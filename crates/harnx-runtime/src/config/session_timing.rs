//! Durable timing context at the shared pre-model boundary.

use super::session_persistence::{detached_sink, SessionAppendSink};
use super::{Config, GlobalConfig, Input};
use crate::nats_session_metadata::{InvocationIdentity, RunLimitsRecord};
use crate::timing_notice::{evaluate_timing, TimingInput, TimingState};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use harnx_core::input::TimingContext;
use harnx_core::message::{Message, MessageContent, MessageRole};
use harnx_core::session::SessionLogEntry;
use harnx_core::timing_identity::{TimingCacheKey, TimingReceipt};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TimingRun {
    pub(crate) state: TimingState,
    // Keep the sink allocation alive so an allocator can't reuse its address
    // and accidentally make a different store match this runtime-only key.
    storage: std::sync::Weak<dyn SessionAppendSink>,
    // Freeze the admitted origin/deadline for this invocation, including retries.
    pub(crate) limits: Option<RunLimitsRecord>,
    pending: Option<PendingNotice>,
    // Only the next logical entry may reuse an unmanaged raw-log replay.
    resume_unmanaged: bool,
    // Explicit evaluator time lets loop tests cross milestones without sleeps
    // or mutating the logical run's frozen origin/deadline.
    #[cfg(test)]
    pub(crate) evaluation_at: Option<DateTime<Utc>>,
}

impl TimingRun {
    fn can_resume_unmanaged(&self) -> bool {
        self.limits.is_none() && self.resume_unmanaged
    }

    fn matches_store(&self, key: &TimingCacheKey, sink: &Arc<dyn SessionAppendSink>) -> bool {
        self.state.matches_key(key)
            && self
                .storage
                .upgrade()
                .is_some_and(|stored| Arc::ptr_eq(&stored, sink))
    }
}

#[derive(Debug, Clone)]
struct PendingNotice {
    context: TimingContext,
    entry: SessionLogEntry,
}

fn cache_key(
    sink: &Arc<dyn SessionAppendSink>,
    session_key: String,
    invocation_id: &InvocationIdentity,
) -> TimingCacheKey {
    // The sink identifies the actual attached storage, even when two brokers
    // have the same session key. Cloned configs retain that same Arc.
    TimingCacheKey::new(
        format!("{:p}", Arc::as_ptr(sink)),
        session_key,
        invocation_id.as_str().to_string(),
    )
}

/// Called once on logical run entry, before embeddings or hooks. Admitted
/// continuations keep their cache. Unmanaged entries start a fresh clock unless
/// an existing raw-log replay hydrated its origin for this entry.
pub(crate) fn start_run(config: &GlobalConfig, now: DateTime<Utc>) {
    let mut guard = config.write();
    let Some((sink, session_key)) = attached_store(&guard) else {
        guard.timing_run = None;
        return;
    };
    let limits = guard.run_context.clone();
    let (invocation, continuing) = invocation_for_entry(limits.as_ref(), guard.timing_run.as_ref());
    let key = cache_key(&sink, session_key, &invocation);
    if let Some(run) = guard
        .timing_run
        .as_mut()
        .filter(|run| continuing && run.matches_store(&key, &sink))
    {
        run.resume_unmanaged = false;
        return;
    }
    let state = if limits.is_some() {
        TimingState::new(key)
    } else {
        TimingState::with_fallback_origin(key, now)
    };
    guard.timing_run = Some(TimingRun {
        state,
        storage: Arc::downgrade(&sink),
        limits,
        pending: None,
        resume_unmanaged: false,
        #[cfg(test)]
        evaluation_at: None,
    });
}

fn attached_store(config: &Config) -> Option<(Arc<dyn SessionAppendSink>, String)> {
    let session = config.session.as_ref()?;
    Some((detached_sink(session)?, session.storage_key()))
}

fn invocation_for_entry(
    limits: Option<&RunLimitsRecord>,
    run: Option<&TimingRun>,
) -> (InvocationIdentity, bool) {
    if let Some(limits) = limits {
        return (limits.invocation_id.clone(), true);
    }
    match run.filter(|run| run.can_resume_unmanaged()) {
        Some(run) => (
            InvocationIdentity::from_string(run.state.key.invocation_id.clone()),
            true,
        ),
        None => (InvocationIdentity::default(), false),
    }
}

/// Read receipts before Compress/EditEntries reduce the model-visible context.
/// Admissions, not receipt prose, supply managed origin and deadline.
pub(crate) fn recover_progress(
    entries: &[(u64, SessionLogEntry)],
    limits: Option<&RunLimitsRecord>,
) -> Option<TimingReceipt> {
    let mut recovery = TimingRecovery {
        limits,
        seen: std::collections::HashSet::new(),
        progress: None,
    };
    for (_, entry) in entries {
        recovery.apply(entry);
    }
    recovery.progress
}

struct TimingRecovery<'a> {
    limits: Option<&'a RunLimitsRecord>,
    seen: std::collections::HashSet<&'a str>,
    progress: Option<TimingReceipt>,
}

impl<'a> TimingRecovery<'a> {
    fn apply(&mut self, entry: &'a SessionLogEntry) {
        // Unmanaged replay has no immutable invocation binding. Stable message
        // IDs distinguish compaction copies from new prompts in its open turn.
        if self.is_unmanaged_duplicate(entry) {
            return;
        }
        if let SessionLogEntry::Message { role, content, .. } = entry {
            self.apply_message(role, content);
        } else if self.limits.is_none() && ends_unmanaged_timing(entry) {
            self.progress = None;
        }
    }

    fn is_unmanaged_duplicate(&mut self, entry: &'a SessionLogEntry) -> bool {
        if self.limits.is_some() {
            return false;
        }
        match entry {
            SessionLogEntry::Message { id: Some(id), .. } => !self.seen.insert(id),
            _ => false,
        }
    }

    fn apply_message(&mut self, role: &MessageRole, content: &MessageContent) {
        // Never restore a completed unmanaged conversation's old clock.
        if role.is_assistant() {
            self.clear_unmanaged_progress();
            return;
        }
        if !role.is_user() {
            return;
        }
        let Some(receipt) = TimingReceipt::from_notice_line(&content.to_text()) else {
            self.clear_unmanaged_progress();
            return;
        };
        if valid_recovered_receipt(&receipt, self.limits) {
            self.merge(receipt);
        }
    }

    fn clear_unmanaged_progress(&mut self) {
        if self.limits.is_none() {
            self.progress = None;
        }
    }

    fn merge(&mut self, receipt: TimingReceipt) {
        if let Some(previous) = self.progress.as_mut().filter(|previous| {
            previous.invocation_id == receipt.invocation_id && previous.origin == receipt.origin
        }) {
            // Re-logged compaction suffixes may contain older receipts after
            // newer ones. Hiding context doesn't undo a durable emission.
            previous.coverage |= receipt.coverage;
            previous.highest_elapsed_bucket = previous
                .highest_elapsed_bucket
                .max(receipt.highest_elapsed_bucket);
        } else {
            self.progress = Some(receipt);
        }
    }
}

fn ends_unmanaged_timing(entry: &SessionLogEntry) -> bool {
    matches!(
        entry,
        SessionLogEntry::Cancel { .. }
            | SessionLogEntry::TurnEnd { .. }
            | SessionLogEntry::Error { .. }
            | SessionLogEntry::Clear
            | SessionLogEntry::Rewind { .. }
    )
}

fn receipt_matches_limits(receipt: &TimingReceipt, limits: &RunLimitsRecord) -> bool {
    receipt.invocation_id == limits.invocation_id.as_str() && receipt.origin == limits.admitted_at
}

fn valid_recovered_receipt(receipt: &TimingReceipt, limits: Option<&RunLimitsRecord>) -> bool {
    use harnx_core::timing_identity::{MilestoneCoverage, TIMING_SCHEMA_VERSION};
    receipt.version == TIMING_SCHEMA_VERSION
        && receipt.coverage.contains(MilestoneCoverage::INITIAL)
        && limits.is_none_or(|limits| receipt_matches_limits(receipt, limits))
}

/// Bind raw progress to the newly attached store, before logical loop entry.
pub(crate) fn hydrate_run(config: &GlobalConfig, receipt: Option<TimingReceipt>) {
    let mut guard = config.write();
    // A reload attaches a new sink; never retain pending writes from the old one.
    guard.timing_run = None;
    let Some(receipt) = receipt else { return };
    let Some(session) = guard.session.as_ref() else {
        return;
    };
    let Some(sink) = detached_sink(session) else {
        return;
    };
    let limits = guard.run_context.clone();
    if limits
        .as_ref()
        .is_some_and(|limits| !receipt_matches_limits(&receipt, limits))
    {
        return;
    }
    let invocation = InvocationIdentity::from_string(receipt.invocation_id.clone());
    let key = cache_key(&sink, session.storage_key(), &invocation);
    let mut state = if limits.is_some() {
        TimingState::new(key)
    } else {
        TimingState::with_fallback_origin(key, receipt.origin)
    };
    state.last_receipt = Some(receipt);
    guard.timing_run = Some(TimingRun {
        state,
        storage: Arc::downgrade(&sink),
        resume_unmanaged: limits.is_none(),
        limits,
        pending: None,
        #[cfg(test)]
        evaluation_at: None,
    });
}

fn same_receipt(message: &Message, receipt: &TimingReceipt) -> bool {
    message.role.is_user()
        && TimingReceipt::from_notice_line(&message.content.to_text()).as_ref() == Some(receipt)
}

fn pending_notice(run: &TimingRun, now: DateTime<Utc>) -> Option<PendingNotice> {
    #[cfg(test)]
    let now = run.evaluation_at.unwrap_or(now);
    if run
        .limits
        .as_ref()
        .is_some_and(|limits| limits.is_expired_at(now))
    {
        return None;
    }
    if let Some(pending) = &run.pending {
        return Some(pending.clone());
    }
    let mut input = if let Some(limits) = &run.limits {
        TimingInput::from_run_limits(now, limits.clone())
    } else {
        TimingInput::unmanaged(
            now,
            run.state.fallback_origin?,
            InvocationIdentity::from_string(run.state.key.invocation_id.clone()),
        )
    };
    input.committed_progress = run.state.last_receipt.clone();
    let output = evaluate_timing(&input);
    let receipt = output.receipt?;
    let content = MessageContent::Text(output.notice_line?);
    // Stable across preparation retries; the pending entry also retains its
    // original timestamp/prose so the existing fenced writer can recognise it.
    let id = format!(
        "{}{}",
        harnx_core::timing_identity::TIMING_MESSAGE_ID_PREFIX,
        harnx_core::crypto::sha256(&receipt.to_json())
    );
    Some(PendingNotice {
        context: TimingContext {
            receipt,
            message: Message::new(MessageRole::User, content.clone()).with_id(id.clone()),
        },
        entry: SessionLogEntry::Message {
            id: Some(id),
            role: MessageRole::User,
            content,
            timestamp: Some(now),
            fence_token: None,
        },
    })
}

/// Persist after initiating/concurrent input, then attach a request suffix.
/// No config guard survives the sink call. Failed appends leave progress and
/// history unchanged, and retain the candidate for the next preparation attempt.
pub(crate) fn prepare_notice(
    config: &GlobalConfig,
    input: &mut Input,
    now: DateTime<Utc>,
) -> Result<()> {
    if input.timing_context.is_some() {
        return Ok(());
    }
    if !input.with_session() {
        return Ok(());
    }
    let Some(append) = stage_notice(config, now)? else {
        return Ok(());
    };
    let seq = if append.existing.is_some() {
        None
    } else {
        Some(
            append
                .sink
                .append(&append.pending.entry)
                .context("persist timing notice")?,
        )
    };

    commit_notice(config, append, seq, input)
}

struct NoticeAppend {
    sink: Arc<dyn SessionAppendSink>,
    key: TimingCacheKey,
    pending: PendingNotice,
    existing: Option<Message>,
}

fn stage_notice(config: &GlobalConfig, now: DateTime<Utc>) -> Result<Option<NoticeAppend>> {
    let mut guard = config.write();
    if guard.dry_run {
        return Ok(None);
    }
    let Some(session) = guard.session.as_ref() else {
        return Ok(None);
    };
    let Some(run) = guard.timing_run.as_ref() else {
        return Ok(None);
    };
    let sink = detached_sink(session).context("timing notice has no persistence sink")?;
    let invocation = InvocationIdentity::from_string(run.state.key.invocation_id.clone());
    let key = cache_key(&sink, session.storage_key(), &invocation);
    anyhow::ensure!(run.state.matches_key(&key), "timing notice session changed");
    let Some(pending) = pending_notice(run, now) else {
        return Ok(None);
    };
    let existing = session
        .messages
        .iter()
        .find(|message| same_receipt(message, &pending.context.receipt))
        .cloned();
    guard.timing_run.as_mut().unwrap().pending = Some(pending.clone());
    Ok(Some(NoticeAppend {
        sink,
        key,
        pending,
        existing,
    }))
}

fn commit_notice(
    config: &GlobalConfig,
    append: NoticeAppend,
    seq: Option<u64>,
    input: &mut Input,
) -> Result<()> {
    let NoticeAppend {
        sink,
        key,
        pending,
        existing,
    } = append;
    let mut context = pending.context;
    if let Some(message) = existing {
        context.message = message;
    }
    let mut guard = config.write();
    anyhow::ensure!(
        guard
            .timing_run
            .as_ref()
            .is_some_and(|run| run.state.matches_key(&key)),
        "timing notice invocation changed during append"
    );
    let session = guard
        .session
        .as_mut()
        .context("timing notice session detached during append")?;
    let current_sink =
        detached_sink(session).context("timing notice sink detached during append")?;
    anyhow::ensure!(
        session.storage_key() == key.session_id && Arc::ptr_eq(&sink, &current_sink),
        "timing notice session changed during append"
    );
    if let Some(seq) = seq {
        context.message.log_seq = Some(seq as usize);
        session.log_entry_count = session.log_entry_count.max(seq as usize);
        session.messages.push(context.message.clone());
        session.update_tokens();
    }
    let run = guard.timing_run.as_mut().unwrap();
    run.state.last_receipt = Some(context.receipt.clone());
    run.pending = None;
    input.timing_context = Some(context);
    Ok(())
}

#[cfg(test)]
mod tests;
