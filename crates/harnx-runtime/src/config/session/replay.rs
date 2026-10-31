//! Reconstruct model-facing session messages from durable log events.

mod runtime_note;

use super::{persisted_message_id, Message, MessageContent, MessageRole, Session, SessionLogEntry};
use anyhow::{Context, Result};
use runtime_note::RuntimeNote;
use serde::Deserialize;

struct PendingToolCalls {
    seq: usize,
    text: String,
    thought: Option<String>,
    calls: Vec<crate::tool::ToolCall>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

/// Marks a message replay synthesizes as informational context for the
/// model, not a prompt the user typed. Public so callers replaying a
/// session (tests included) can tell runtime notes apart from real
/// user-authored history despite both carrying `MessageRole::User`.
pub use runtime_note::RUNTIME_NOTE_PREFIX;

pub(super) const LOST_TOOL_RESPONSE_ERROR: &str =
    "tool response lost (session was interrupted before results were persisted)";
pub(super) const PENDING_TOOL_RESPONSE_ERROR: &str =
    "tool response pending (results not yet persisted)";
/// Public only so integration tests can name the placeholder a wind-up
/// writes instead of repeating its text; not part of the crate's API.
#[doc(hidden)]
pub const INTERRUPTED_TOOL_RESPONSE_ERROR: &str = "tool call interrupted by user";

/// Synthesize placeholder results for tool calls cut off by a user
/// interruption. The worker's wind-up persists these as the `ToolResults`
/// entry that closes out the round the `Cancel` entry terminated, for every
/// call the invocation journal has no real reply for.
pub(crate) fn interrupted_tool_outputs(
    calls: &[harnx_core::tool::ToolCall],
    cancellation_id: Option<&str>,
) -> Vec<harnx_core::session::ToolOutput> {
    calls
        .iter()
        .map(|call| harnx_core::session::ToolOutput {
            id: call.id.clone(),
            name: call.name.clone(),
            output: serde_json::json!({
                "error": INTERRUPTED_TOOL_RESPONSE_ERROR,
                "cancellation_id": cancellation_id,
            }),
            markdown: None,
            content: Vec::new(),
            switch_agent: None,
        })
        .collect()
}

pub(super) struct LogSource<'a> {
    pub(super) content: &'a str,
    pub(super) name: &'a str,
}

impl LogSource<'_> {
    pub(super) fn entries(&self) -> Result<Vec<(usize, SessionLogEntry)>> {
        serde_yaml::Deserializer::from_str(self.content)
            .enumerate()
            .map(|(seq, document)| {
                let entry = SessionLogEntry::deserialize(document).with_context(|| {
                    format!("Invalid log entry #{seq} in session {}", self.name)
                })?;
                Ok((seq, entry))
            })
            .collect()
    }
}

fn build_effective_log_entries(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
) -> Vec<(usize, SessionLogEntry)> {
    // Delegate to the canonical implementation in harnx-core
    harnx_core::session_reconstruct::apply_log_mutations_with_name(raw_entries, name)
}

/// Replay a local-file transcript. Legacy Header/Title rows (which
/// deserialize as `Unknown`) are tolerated here for compatibility with older
/// logs. When `preserve_pending` is set, a trailing tool-call batch is kept as
/// pending placeholders instead of being closed out with interrupted results;
/// read-only observers pass `true` while a worker still holds the lease.
pub fn replay_log_entries_for_external(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
    preserve_pending: bool,
) -> Result<Session> {
    replay_log_entries_into_session(
        raw_entries,
        name,
        Session::default(),
        ReplayOptions::new(true, preserve_pending),
    )
}

/// Replay conversation-only NATS entries into state initialized from canonical
/// session metadata. Header and title rows are protocol violations in this
/// path: their state belongs in KV and must never be synthesized or repaired
/// from transcript contents. When `preserve_pending` is set, a trailing
/// tool-call batch is kept as pending placeholders instead of being closed out
/// with interrupted results; read-only observers pass `true` while a worker
/// still holds the lease.
pub fn replay_nats_entries_into_session(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
    session: Session,
    preserve_pending: bool,
) -> Result<Session> {
    replay_log_entries_into_session(
        raw_entries,
        name,
        session,
        ReplayOptions::new(false, preserve_pending),
    )
}

#[derive(Clone, Copy)]
enum TrailingToolCallPolicy {
    Interrupted,
    Pending,
}

struct ReplayOptions {
    allow_embedded_metadata: bool,
    trailing_tool_calls: TrailingToolCallPolicy,
}

impl ReplayOptions {
    fn new(allow_embedded_metadata: bool, preserve_pending: bool) -> Self {
        Self {
            allow_embedded_metadata,
            trailing_tool_calls: if preserve_pending {
                TrailingToolCallPolicy::Pending
            } else {
                TrailingToolCallPolicy::Interrupted
            },
        }
    }
}

fn replay_log_entries_into_session(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
    session: Session,
    options: ReplayOptions,
) -> Result<Session> {
    // `apply_log_mutations_with_name` intentionally ignores unknown rows for
    // tolerant local-file replay. Canonical NATS transcripts are a hard
    // protocol boundary, so reject legacy Header/Title rows (which deserialize
    // as `Unknown`) before mutation resolution can discard them.
    if !options.allow_embedded_metadata {
        anyhow::ensure!(
            !raw_entries
                .iter()
                .any(|(_, entry)| matches!(entry, SessionLogEntry::Unknown)),
            "unsupported or legacy entry found in canonical NATS transcript '{name}'"
        );
    }
    let effective_entries = build_effective_log_entries(raw_entries, name);
    let mut replay = ReplayState::new(session, name);
    for (seq, entry) in effective_entries {
        if let Some(note) = RuntimeNote::from_entry(&entry) {
            replay.runtime_note(note, seq);
        } else {
            replay.apply_entry(seq, entry, options.allow_embedded_metadata)?;
        }
    }
    replay.finish(options.trailing_tool_calls)
}

struct ReplayState<'a> {
    session: Session,
    name: &'a str,
    // Buffer user messages and runtime notes until the result arrives, preserving
    // assistant tool call -> tool result -> queued message order.
    pending: Option<PendingToolCalls>,
    messages_queued_during_tool: Vec<Message>,
    completed_tool_call_ids: std::collections::HashSet<String>,
}

impl<'a> ReplayState<'a> {
    fn new(session: Session, name: &'a str) -> Self {
        Self {
            session,
            name,
            pending: None,
            messages_queued_during_tool: Vec::new(),
            completed_tool_call_ids: std::collections::HashSet::new(),
        }
    }

    fn apply_entry(
        &mut self,
        seq: usize,
        entry: SessionLogEntry,
        allow_embedded_metadata: bool,
    ) -> Result<()> {
        match entry {
            SessionLogEntry::Message {
                id,
                role,
                content,
                timestamp,
                ..
            } => {
                self.message(Message {
                    role,
                    content,
                    id,
                    log_seq: Some(seq),
                    log_timestamp: timestamp,
                })?;
            }
            SessionLogEntry::ToolCalls {
                text,
                thought,
                calls,
                timestamp,
                ..
            } => {
                self.tool_calls(PendingToolCalls {
                    seq,
                    text,
                    thought,
                    calls,
                    timestamp,
                })?;
            }
            SessionLogEntry::ToolResults { results, .. } => self.tool_results(seq, results),
            SessionLogEntry::DataUrls { urls } => self.session.data_urls.extend(urls),
            SessionLogEntry::Compress { prompt } => self.compress(prompt)?,
            SessionLogEntry::Clear => self.clear(),
            SessionLogEntry::Unknown => anyhow::ensure!(
                allow_embedded_metadata,
                "unsupported or legacy entry found in canonical NATS transcript '{}'",
                self.name
            ),
            // Errors annotate the transcript; control state is hydrated separately.
            // None of these entries may break pending tool-call/result adjacency.
            // Runtime-note entries were handled before this dispatch.
            SessionLogEntry::SubAgentStarted { .. }
            | SessionLogEntry::Cancel { .. }
            | SessionLogEntry::Error { .. }
            | SessionLogEntry::TurnEnd { .. }
            | SessionLogEntry::HandoffCommitted { .. }
            | SessionLogEntry::HitlApprovalRequested { .. }
            | SessionLogEntry::HitlApprovalDecision { .. }
            | SessionLogEntry::CompactRequest { .. }
            | SessionLogEntry::CompactResult { .. }
            | SessionLogEntry::AdmissionClosed { .. }
            | SessionLogEntry::EditEntries { .. }
            | SessionLogEntry::Rewind { .. } => {}
        }
        Ok(())
    }

    fn message(&mut self, message: Message) -> Result<()> {
        if message.role == MessageRole::Tool {
            anyhow::bail!(
                "Invalid log entry in session {}: Tool-role Message entries are                          no longer supported; use tool_calls/tool_results entries", self.name
            );
        }
        if message.role == MessageRole::System && !self.session.agent_instructions.is_empty() {
            return Ok(());
        }
        if message.role.is_user() && self.pending.is_some() {
            self.messages_queued_during_tool.push(message);
            return Ok(());
        }
        self.finish_orphaned_tool_round()?;
        self.session.messages.push(message);
        Ok(())
    }

    fn runtime_note(&mut self, note: RuntimeNote<'_>, seq: usize) {
        let message = note.into_message(seq);
        if self.pending.is_some() {
            self.messages_queued_during_tool.push(message);
        } else {
            self.session.messages.push(message);
        }
    }

    fn tool_calls(&mut self, pending: PendingToolCalls) -> Result<()> {
        self.finish_orphaned_tool_round()?;
        for call in &pending.calls {
            if let Some(id) = &call.id {
                // Providers should issue unique IDs, but treating a new call as
                // authoritative avoids hiding a result if an old ID is reused.
                self.completed_tool_call_ids.remove(id);
            }
        }
        self.pending = Some(pending);
        Ok(())
    }

    fn tool_results(&mut self, seq: usize, results: Vec<harnx_core::session::ToolOutput>) {
        let Some(PendingToolCalls {
            seq: call_seq,
            text,
            thought,
            calls,
            timestamp,
        }) = self.pending.take()
        else {
            self.orphaned_tool_results(seq, &results);
            return;
        };
        self.completed_tool_call_ids
            .extend(calls.iter().filter_map(|call| call.id.clone()));
        let mut message =
            assemble_tool_message(text, thought, calls, results).with_log_seq(call_seq);
        if let Some(timestamp) = timestamp {
            message = message.with_log_timestamp(timestamp);
        }
        self.session.messages.push(message);
        self.session
            .messages
            .append(&mut self.messages_queued_during_tool);
    }

    fn orphaned_tool_results(&mut self, seq: usize, results: &[harnx_core::session::ToolOutput]) {
        let duplicate = !results.is_empty()
            && results.iter().all(|result| {
                result
                    .id
                    .as_ref()
                    .is_some_and(|id| self.completed_tool_call_ids.contains(id))
            });
        if duplicate {
            log::debug!(
                "Ignored duplicate tool results at log entry {seq} in session {}",
                self.name
            );
            return;
        }
        let warning = format!(
            "Ignored orphan tool results at log entry {seq} in session {}; the corresponding tool call was missing or interrupted", self.name
        );
        log::warn!("{warning}");
        self.session.replay_warnings.push(warning);
    }

    fn compress(&mut self, prompt: String) -> Result<()> {
        self.finish_orphaned_tool_round()?;
        self.session
            .compressed_messages
            .append(&mut self.session.messages);
        self.session.compaction_summary = Some(prompt);
        Ok(())
    }

    fn clear(&mut self) {
        self.pending = None;
        self.messages_queued_during_tool.clear();
        self.completed_tool_call_ids.clear();
        self.session.messages.clear();
        self.session.compressed_messages.clear();
        self.session.data_urls.clear();
        // The summary describes archived rows; with nothing left in the
        // session it would be stale context injected into the next request.
        self.session.compaction_summary = None;
    }

    fn finish_orphaned_tool_round(&mut self) -> Result<()> {
        if let Some(pending) = self.pending.take() {
            self.session
                .messages
                .push(repair_orphan_tool_calls(pending, self.name)?);
        }
        self.session
            .messages
            .append(&mut self.messages_queued_during_tool);
        Ok(())
    }

    fn finish(mut self, trailing_tool_calls: TrailingToolCallPolicy) -> Result<Session> {
        if let Some(pending) = self.pending.take() {
            let message = match trailing_tool_calls {
                TrailingToolCallPolicy::Interrupted => {
                    repair_orphan_tool_calls(pending, self.name)?
                }
                TrailingToolCallPolicy::Pending => preserve_pending_tool_calls(pending),
            };
            self.session.messages.push(message);
        }
        self.session
            .messages
            .append(&mut self.messages_queued_during_tool);
        Ok(self.session)
    }
}

/// Test-only log parser — runs full load pipeline (including replay and
/// orphan-ToolCalls repair) but skips model-catalog lookup that
/// `super::super::load` performs, so it works against minimal `Config::default`
/// used in unit tests.
#[cfg(test)]
pub(crate) fn load_from_log_for_test(content: &str) -> Session {
    let raw_entries = LogSource {
        content,
        name: "test",
    }
    .entries()
    .expect("valid log entries");
    let mut session = replay_log_entries_for_external(&raw_entries, "test", false)
        .expect("replay should succeed");
    session.log_entry_count = raw_entries.len();
    session
}

fn repair_orphan_tool_calls(pending: PendingToolCalls, _name: &str) -> Result<Message> {
    Ok(assemble_synthetic_tool_message(
        pending,
        LOST_TOOL_RESPONSE_ERROR,
    ))
}

fn preserve_pending_tool_calls(pending: PendingToolCalls) -> Message {
    assemble_synthetic_tool_message(pending, PENDING_TOOL_RESPONSE_ERROR)
}

fn assemble_synthetic_tool_message(pending: PendingToolCalls, error: &str) -> Message {
    let PendingToolCalls {
        seq,
        text,
        thought,
        calls,
        timestamp,
    } = pending;
    let lost = harnx_core::session::ToolOutput {
        id: None,
        name: String::new(),
        output: serde_json::json!({ "error": error }),
        markdown: None,
        content: Vec::new(),
        switch_agent: None,
    };
    // Fabricate one lost-response per call, matched by id/position.
    let results: Vec<_> = calls
        .iter()
        .map(|c| harnx_core::session::ToolOutput {
            id: c.id.clone(),
            name: c.name.clone(),
            ..lost.clone()
        })
        .collect();
    let mut message = assemble_tool_message(text, thought, calls, results)
        .with_id(persisted_message_id())
        .with_log_seq(seq);
    if let Some(timestamp) = timestamp {
        message = message.with_log_timestamp(timestamp);
    }
    message
}

/// Whether a reconstructed tool result is only the placeholder for a call
/// that an active lease holder has not finished yet.
pub fn is_pending_tool_result(result: &crate::tool::ToolResult) -> bool {
    result.output.as_object().is_some_and(|output| {
        output.len() == 1
            && output.get("error").and_then(serde_json::Value::as_str)
                == Some(PENDING_TOOL_RESPONSE_ERROR)
    })
}

fn assemble_tool_message(
    text: String,
    thought: Option<String>,
    calls: Vec<crate::tool::ToolCall>,
    results: Vec<harnx_core::session::ToolOutput>,
) -> Message {
    use crate::client::MessageContentToolCalls;
    use crate::tool::ToolResult;

    // Match each call to its result by id (falling back to position).
    let mut by_id: std::collections::HashMap<String, harnx_core::session::ToolOutput> = results
        .iter()
        .filter_map(|r| r.id.clone().map(|id| (id, r.clone())))
        .collect();
    let mut positional = results.into_iter().filter(|r| r.id.is_none());

    let tool_results: Vec<ToolResult> = calls
        .into_iter()
        .map(|call| {
            let output = match call
                .id
                .as_ref()
                .and_then(|id| by_id.remove(id))
                .or_else(|| positional.next())
            {
                Some(out) => ToolResult {
                    call,
                    output: out.output,
                    markdown: out.markdown,
                    content: out.content,
                    switch_agent: out.switch_agent,
                    execution_context: None,
                },
                None => ToolResult::new(
                    call,
                    serde_json::json!({
                        "error": LOST_TOOL_RESPONSE_ERROR
                    }),
                ),
            };
            output
        })
        .collect();

    Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(MessageContentToolCalls::new(tool_results, text, thought)),
    )
}
