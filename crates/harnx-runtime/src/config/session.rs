use super::session_externalize::{
    externalize_content, externalize_tool_result_content, record_externalized,
};
#[cfg(test)]
pub(crate) use super::session_persistence::attach_memory_log;
use super::session_persistence::require_authoritative_appends;
pub use super::session_persistence::{
    append_event, persist_execution_contexts, persist_session_override, persist_session_overrides,
    record_title, session_overrides, SessionAppendSink,
};
use super::*;
use crate::nats_session::new_client_message_id;

pub use harnx_core::session::{Session, SessionLogEntry};

#[cfg(test)]
use std::sync::Arc;

use crate::client::{CompletionTokenUsage, Message, MessageContent, MessageRole};
use harnx_core::{
    event::{AgentEvent, SessionEvent},
    sink::emit_agent_event,
};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;

use crate::utils::session_name::{decode_timestamp_session_id, generate_session_id};

pub fn new(config: &Config, name: &str, working_dir: Option<&std::path::Path>) -> Result<Session> {
    let agent = config.extract_agent();
    let session_id = if uuid::Uuid::parse_str(name)
        .ok()
        .is_some_and(|uuid| uuid.get_version_num() == 7)
        || decode_timestamp_session_id(name).is_some()
    {
        name.to_string()
    } else {
        generate_session_id(|_| false)
    };
    let mut session = Session {
        id: name.to_string(),
        session_id: Some(session_id),
        working_dir: working_dir.map(|path| path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    session.set_agent(&agent)?;
    session.dirty = false;
    Ok(session)
}

struct PendingToolCalls {
    seq: usize,
    text: String,
    thought: Option<String>,
    calls: Vec<crate::tool::ToolCall>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

const LOST_TOOL_RESPONSE_ERROR: &str =
    "tool response lost (session was interrupted before results were persisted)";
const PENDING_TOOL_RESPONSE_ERROR: &str = "tool response pending (results not yet persisted)";

pub(crate) fn collect_raw_log_entries(
    content: &str,
    name: &str,
) -> Result<Vec<(usize, SessionLogEntry)>> {
    serde_yaml::Deserializer::from_str(content)
        .enumerate()
        .map(|(seq, document)| {
            let entry = SessionLogEntry::deserialize(document)
                .with_context(|| format!("Invalid log entry #{seq} in session {name}"))?;
            Ok((seq, entry))
        })
        .collect()
}

fn build_effective_log_entries(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
) -> Vec<(usize, SessionLogEntry)> {
    // Delegate to the canonical implementation in harnx-core
    harnx_core::session_reconstruct::apply_log_mutations_with_name(raw_entries, name)
}

pub fn replay_log_entries_for_external(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
) -> Result<Session> {
    replay_log_entries_into_session(
        raw_entries,
        name,
        Session::default(),
        true,
        TrailingToolCallPolicy::Interrupted,
    )
}

/// Replay conversation-only NATS entries into state initialized from canonical
/// session metadata. Header and title rows are protocol violations in this
/// path: their state belongs in KV and must never be synthesized or repaired
/// from transcript contents.
pub fn replay_nats_entries_into_session(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
    session: Session,
) -> Result<Session> {
    replay_nats_entries_into_session_with_policy(
        raw_entries,
        name,
        session,
        TrailingToolCallPolicy::Interrupted,
    )
}

/// Replay a NATS transcript for a read-only observer while its worker lease is
/// held. A trailing tool-call batch is still in flight in this case, so retain
/// pending placeholders instead of fabricating interrupted results.
pub fn replay_nats_entries_into_session_preserving_pending(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
    session: Session,
) -> Result<Session> {
    replay_nats_entries_into_session_with_policy(
        raw_entries,
        name,
        session,
        TrailingToolCallPolicy::Pending,
    )
}

fn replay_nats_entries_into_session_with_policy(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
    session: Session,
    trailing_tool_calls: TrailingToolCallPolicy,
) -> Result<Session> {
    // `apply_log_mutations_with_name` intentionally ignores unknown rows for
    // tolerant local-file replay. Canonical NATS transcripts are a hard
    // protocol boundary, so reject legacy Header/Title rows (which deserialize
    // as `Unknown`) before mutation resolution can discard them.
    anyhow::ensure!(
        !raw_entries
            .iter()
            .any(|(_, entry)| matches!(entry, SessionLogEntry::Unknown)),
        "unsupported or legacy entry found in canonical NATS transcript '{name}'"
    );
    replay_log_entries_into_session(raw_entries, name, session, false, trailing_tool_calls)
}

#[derive(Clone, Copy)]
enum TrailingToolCallPolicy {
    Interrupted,
    Pending,
}

fn replay_log_entries_into_session(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
    mut session: Session,
    allow_embedded_metadata: bool,
    trailing_tool_calls: TrailingToolCallPolicy,
) -> Result<Session> {
    let effective_entries = build_effective_log_entries(raw_entries, name);

    // Pending ToolCalls entry awaiting a matching ToolResults entry. User
    // messages may be appended while the tool is still running, so buffer
    // them until the result arrives and replay the valid conversation order:
    // assistant tool call -> tool result -> injected user message.
    let mut pending: Option<PendingToolCalls> = None;
    let mut users_queued_during_tool = Vec::new();
    let mut completed_tool_call_ids = std::collections::HashSet::new();

    for (seq, entry) in effective_entries {
        match entry {
            SessionLogEntry::Message {
                id,
                role,
                content,
                timestamp,
                ..
            } => {
                if role == MessageRole::Tool {
                    anyhow::bail!(
                        "Invalid log entry in session {name}: Tool-role Message entries are                          no longer supported; use tool_calls/tool_results entries"

                    );
                }
                if role == MessageRole::System && !session.agent_instructions.is_empty() {
                    continue;
                }
                let mut message = Message::new(role, content).with_log_seq(seq);
                if let Some(id) = id {
                    message = message.with_id(id);
                }
                if let Some(timestamp) = timestamp {
                    message = message.with_log_timestamp(timestamp);
                }
                if role.is_user() && pending.is_some() {
                    users_queued_during_tool.push(message);
                    continue;
                }
                finish_orphaned_tool_round(
                    &mut session,
                    &mut pending,
                    &mut users_queued_during_tool,
                    name,
                )?;
                session.messages.push(message);
            }
            SessionLogEntry::ToolCalls {
                text,
                thought,
                calls,
                timestamp,
                ..
            } => {
                finish_orphaned_tool_round(
                    &mut session,
                    &mut pending,
                    &mut users_queued_during_tool,
                    name,
                )?;
                for call in &calls {
                    if let Some(id) = &call.id {
                        // Providers should issue unique IDs, but treating a
                        // newly observed call as authoritative avoids hiding a
                        // later result if an old ID is reused.
                        completed_tool_call_ids.remove(id);
                    }
                }
                pending = Some(PendingToolCalls {
                    seq,
                    text,
                    thought,
                    calls,
                    timestamp,
                });
            }
            SessionLogEntry::ToolResults { results, .. } => {
                let Some(PendingToolCalls {
                    seq,
                    text,
                    thought,
                    calls,
                    timestamp,
                }) = pending.take()
                else {
                    let duplicate = !results.is_empty()
                        && results.iter().all(|result| {
                            result
                                .id
                                .as_ref()
                                .is_some_and(|id| completed_tool_call_ids.contains(id))
                        });
                    if duplicate {
                        log::debug!(
                            "Ignored duplicate tool results at log entry {seq} in session {name}"
                        );
                        continue;
                    }
                    let warning = format!(
                        "Ignored orphan tool results at log entry {seq} in session {name}; the corresponding tool call was missing or interrupted"
                    );
                    log::warn!("{warning}");
                    session.replay_warnings.push(warning);
                    continue;
                };
                completed_tool_call_ids.extend(calls.iter().filter_map(|call| call.id.clone()));
                let mut message =
                    assemble_tool_message(text, thought, calls, results).with_log_seq(seq);
                if let Some(timestamp) = timestamp {
                    message = message.with_log_timestamp(timestamp);
                }
                session.messages.push(message);
                session.messages.append(&mut users_queued_during_tool);
            }
            SessionLogEntry::DataUrls { urls } => {
                session.data_urls.extend(urls);
            }
            SessionLogEntry::Compress { prompt } => {
                finish_orphaned_tool_round(
                    &mut session,
                    &mut pending,
                    &mut users_queued_during_tool,
                    name,
                )?;
                session.compressed_messages.append(&mut session.messages);
                session.compaction_summary = Some(prompt);
            }
            SessionLogEntry::Clear => {
                pending = None;
                users_queued_during_tool.clear();
                completed_tool_call_ids.clear();
                session.messages.clear();
                session.compressed_messages.clear();
                session.data_urls.clear();
            }
            // A failed turn is a transcript annotation, not conversation
            // history — replaying it would feed the error back to the model.
            SessionLogEntry::Error { .. } => {}
            SessionLogEntry::TurnEnd { .. } => {}
            SessionLogEntry::Cancel { .. } => {}
            SessionLogEntry::EditEntries { .. } | SessionLogEntry::Rewind { .. } => {}
            SessionLogEntry::Unknown => anyhow::ensure!(
                allow_embedded_metadata,
                "unsupported or legacy entry found in canonical NATS transcript '{name}'"
            ),
        }
    }

    if let Some(pending) = pending.take() {
        let message = match trailing_tool_calls {
            TrailingToolCallPolicy::Interrupted => repair_orphan_tool_calls(pending, name)?,
            TrailingToolCallPolicy::Pending => preserve_pending_tool_calls(pending),
        };
        session.messages.push(message);
    }
    session.messages.append(&mut users_queued_during_tool);

    Ok(session)
}

fn finish_orphaned_tool_round(
    session: &mut Session,
    pending: &mut Option<PendingToolCalls>,
    users_queued_during_tool: &mut Vec<Message>,
    name: &str,
) -> Result<()> {
    if let Some(pending) = pending.take() {
        session
            .messages
            .push(repair_orphan_tool_calls(pending, name)?);
    }
    session.messages.append(users_queued_during_tool);
    Ok(())
}

/// Test-only log parser — runs full load pipeline (including replay and
/// orphan-ToolCalls repair) but skips model-catalog lookup that
/// `super::load` performs, so it works against minimal `Config::default`
/// used in unit tests.
#[cfg(test)]
pub(crate) fn load_from_log_for_test(content: &str) -> Session {
    let raw_entries = collect_raw_log_entries(content, "test").expect("valid log entries");
    let mut session =
        replay_log_entries_for_external(&raw_entries, "test").expect("replay should succeed");
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
        MessageContent::ToolCalls(MessageContentToolCalls {
            tool_results,
            text,
            thought,
            sequence: false,
        }),
    )
}

pub fn render(session: &Session) -> Result<String> {
    let mut items = vec![];

    items.push((
        "model",
        format!(
            "{} (vision: {})",
            session.model().id(),
            session.model().supports_vision()
        ),
    ));

    let title = match session.title() {
        Some(title) if session.title_last_updated_tokens() == usize::MAX => {
            format!("{title} (manual)")
        }
        Some(title) => title.to_string(),
        None => "(none)".to_string(),
    };
    items.push(("title", title));

    if let Some(temperature) = session.temperature() {
        items.push(("temperature", temperature.to_string()));
    }
    if let Some(top_p) = session.top_p() {
        items.push(("top_p", top_p.to_string()));
    }

    if let Some(use_tools) = session.use_tools() {
        items.push(("use_tools", use_tools.join(",")));
    }

    if !session.model_fallbacks.is_empty() {
        items.push(("model_fallbacks", session.model_fallbacks.join(",")));
    }

    if let Some(compress_threshold) = session.compress_threshold {
        items.push(("compress_threshold", compress_threshold.to_string()));
    }

    if let Some(max_input_tokens) = session.model().max_input_tokens() {
        items.push(("max_input_tokens", max_input_tokens.to_string()));
    }

    let (tokens, percent) = session.tokens_usage();
    let tokens_str = if percent > 0.0 {
        format!("{tokens} ({percent}%)")
    } else {
        tokens.to_string()
    };
    items.push(("tokens", tokens_str));

    let message_count = session.messages.iter().filter(|m| m.role.is_user()).count();
    items.push(("turns", message_count.to_string()));

    let lines: Vec<String> = items
        .iter()
        .map(|(name, value)| format!("{name:<20}{value}"))
        .collect();

    Ok(lines.join("\n"))
}

/// Appends YAML log entry/entries for `msg` to `content`.
///
/// Tool-role messages containing `MessageContent::ToolCalls` are split
/// into a `tool_calls` entry (the LLM's request) followed by a
/// `tool_results` entry (the tool outputs), matching the format that
/// `replay_log_entries` expects. All other messages are written as a
/// single `message` entry.
fn persisted_message_id() -> String {
    new_client_message_id()
}

pub fn to_agent(session: &Session) -> Agent {
    Agent::new(
        session
            .to_agent_config()
            .expect("session agent config should always be valid"),
    )
}

pub fn compress(session: &mut Session, prompt: String) {
    session.compressed_messages.append(&mut session.messages);
    session.compaction_summary = Some(prompt.clone());
    session.update_tokens();
    if !append_event(session, &SessionLogEntry::Compress { prompt }) {
        session.dirty = true;
    }
}

/// Compact only the prefix `messages[..keep_from]`, keeping `messages[keep_from..]`
/// verbatim. The prefix moves to `compressed_messages`; the new message list is
/// just preserved suffix, while summary stays in `session.compaction_summary`.
pub fn compress_keeping_recent(session: &mut Session, prompt: String, keep_from: usize) {
    let keep_from = keep_from.min(session.messages.len());
    // Split off the recent suffix to keep verbatim; the remainder is the prefix.
    let suffix: Vec<Message> = session.messages.split_off(keep_from);
    session.compressed_messages.append(&mut session.messages);
    // Hard-cut log layout: the Compress event archives the prefix and carries
    // the summary; the preserved suffix is then re-logged as fresh entries so
    // replay reproduces active suffix without any stored index.
    session.compaction_summary = Some(prompt.clone());
    if !append_event(session, &SessionLogEntry::Compress { prompt }) {
        session.dirty = true;
    }
    for mut msg in suffix {
        let seq = relog_message(session, &msg);
        msg.log_seq = Some(seq);
        session.messages.push(msg);
    }
    session.update_tokens();
}

/// Re-append an in-memory message to the session log as the entry (or the
/// `ToolCalls`+`ToolResults` pair) it round-trips from, returning the log_seq of
/// its first entry. Used to re-log the preserved suffix after a `Compress`
/// event so the NATS log is self-describing (no stored index).
fn relog_message(session: &mut Session, msg: &Message) -> usize {
    let seq = session.next_seq();
    let message_id = msg.id.clone();
    if msg.role == MessageRole::Tool {
        if let MessageContent::ToolCalls(tc) = &msg.content {
            let calls: Vec<crate::tool::ToolCall> =
                tc.tool_results.iter().map(|r| r.call.clone()).collect();
            let ok_calls = append_event(
                session,
                &SessionLogEntry::ToolCalls {
                    text: tc.text.clone(),
                    thought: tc.thought.clone(),
                    calls,
                    timestamp: msg.log_timestamp,
                    fence_token: None,
                },
            );
            let results: Vec<harnx_core::session::ToolOutput> = tc
                .tool_results
                .iter()
                .map(|r| harnx_core::session::ToolOutput {
                    id: r.call.id.clone(),
                    name: r.call.name.clone(),
                    output: r.output.clone(),
                    markdown: r.markdown.clone(),
                    content: r.content.clone(),
                    switch_agent: r.switch_agent.clone(),
                })
                .collect();
            let ok_results = append_event(
                session,
                &SessionLogEntry::ToolResults {
                    results,
                    timestamp: msg.log_timestamp,
                },
            );
            if !(ok_calls && ok_results) {
                session.dirty = true;
            }
            return seq;
        }
    }
    if !append_event(
        session,
        &SessionLogEntry::Message {
            id: message_id,
            role: msg.role,
            content: msg.content.clone(),
            timestamp: msg.log_timestamp,
            fence_token: None,
        },
    ) {
        session.dirty = true;
    }
    seq
}

/// Record an assistant turn that produced plain text (no tool calls).
/// Handles the first-turn agent setup, optional user-message push, and
/// continue/regenerate edit modes.  Exactly one `Message(Assistant,
/// Text)` log entry is appended.
pub fn add_assistant_text(
    session: &mut Session,
    input: &Input,
    output: &str,
    thought: Option<&str>,
) -> Result<()> {
    if input.continue_output().is_some() {
        if let Some(message) = session.messages.last_mut() {
            if let MessageContent::Text(text) = &mut message.content {
                *text = format!("{text}{output}");
            }
        }
        session.dirty = true;
    } else if input.regenerate() {
        if let Some(message) = session.messages.last_mut() {
            if let MessageContent::Text(text) = &mut message.content {
                *text = output.to_string();
            }
        }
        session.dirty = true;
    } else {
        let mut all_appended = begin_turn(session, input, output)?;
        let content = match thought {
            Some(v) => MessageContent::Text(format!("<think>\n{v}\n</think>\n{output}")),
            _ => MessageContent::Text(output.to_string()),
        };
        let seq = session.next_seq();
        let timestamp = Utc::now();
        let message_id = input
            .preferred_assistant_message_id()
            .map(ToOwned::to_owned)
            .unwrap_or_else(persisted_message_id);
        let assistant_msg = Message::new(MessageRole::Assistant, content)
            .with_id(message_id.clone())
            .with_log_seq(seq)
            .with_log_timestamp(timestamp);
        all_appended &= append_event(
            session,
            &SessionLogEntry::Message {
                id: Some(message_id),
                role: assistant_msg.role,
                content: assistant_msg.content.clone(),
                timestamp: Some(timestamp),
                fence_token: None,
            },
        );
        session.messages.push(assistant_msg);
        emit_agent_event(AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }));
        session.dirty = !all_appended;
        require_authoritative_appends(session, all_appended, "assistant response")?;
    }
    session.update_tokens();
    Ok(())
}

/// Record that the LLM issued tool calls.  Called BEFORE the tools
/// actually execute, so the transcript captures what was requested
/// even if the process is interrupted mid-round.  Writes a
/// `ToolCalls` log entry and pushes a pending in-memory `Tool`
/// message whose outputs are filled in by a matching
/// [`add_tool_results`] call.
pub fn add_tool_calls(
    session: &mut Session,
    input: &Input,
    output: &str,
    thought: Option<&str>,
    calls: &[crate::tool::ToolCall],
) -> Result<()> {
    // Dedup matches what `eval_tool_calls` does before execution. Keeping
    // the two in sync means pending slots, the tool_calls log entry, and
    // the eventual tool_results all describe the same set of calls —
    // otherwise duplicate-id calls from the LLM leave orphan pending
    // slots that persist as "tool response pending" placeholders in the
    // log (issue: multiple results with the same id sent to the LLM).
    let calls = crate::tool::ToolCall::dedup(calls.to_vec());
    let mut all_appended = begin_turn(session, input, output)?;
    let tool_calls_seq = session.next_seq();
    let tool_message_id = persisted_message_id();
    all_appended &= append_event(
        session,
        &SessionLogEntry::ToolCalls {
            text: output.to_string(),
            thought: thought.map(str::to_string),
            calls: calls.clone(),
            timestamp: Some(Utc::now()),
            fence_token: None,
        },
    );
    emit_agent_event(AgentEvent::Session(SessionEvent::LogSeqAssigned {
        seq: tool_calls_seq,
    }));
    // Push a pending Tool message.  Outputs are filled in by
    // add_tool_results; synthetic error placeholders mean that if the
    // pending message ever leaks (e.g. a mid-round abort without a
    // matching add_tool_results call), the next LLM replay sees
    // well-formed content instead of nulls.
    let pending_results: Vec<crate::tool::ToolResult> = calls
        .into_iter()
        .map(|call| {
            crate::tool::ToolResult::new(
                call,
                serde_json::json!({
                    "error": PENDING_TOOL_RESPONSE_ERROR
                }),
            )
        })
        .collect();
    let content = MessageContent::ToolCalls(crate::client::MessageContentToolCalls::new(
        pending_results,
        output.to_string(),
        thought.map(str::to_string),
    ));
    session.messages.push(
        Message::new(MessageRole::Tool, content)
            .with_id(tool_message_id)
            .with_log_seq(tool_calls_seq),
    );
    session.dirty = !all_appended;
    require_authoritative_appends(session, all_appended, "tool calls")?;
    session.update_tokens();
    Ok(())
}

/// Finalize the tool round opened by [`add_tool_calls`] by filling in
/// the in-memory outputs and writing a `ToolResults` log entry.
/// Matches each result to its call by id (or by position when the id
/// is absent).
pub fn add_tool_results(session: &mut Session, results: &[crate::tool::ToolResult]) -> Result<()> {
    // Resolve the attachments dir up front so we don't need to borrow `session`
    // again while the `pending` mutable borrow below is live.
    let attachments_dir = super::session_externalize::attachments_dir(session);
    let mut cid_urls = std::collections::HashMap::new();

    let Some(last) = session.messages.last_mut() else {
        anyhow::bail!("add_tool_results called on empty session");
    };
    let MessageContent::ToolCalls(ref mut pending) = last.content else {
        anyhow::bail!(
            "add_tool_results called but the last session message is not a pending tool-call turn"
        );
    };
    if last.role != MessageRole::Tool {
        anyhow::bail!("add_tool_results called but the last session message is not role=Tool");
    }

    // Match results to the pending calls by id (fallback: position).
    let mut by_id: std::collections::HashMap<String, crate::tool::ToolResult> = results
        .iter()
        .filter_map(|r| r.call.id.clone().map(|id| (id, r.clone())))
        .collect();
    let mut positional = results.iter().filter(|r| r.call.id.is_none()).cloned();
    for slot in pending.tool_results.iter_mut() {
        let replacement = slot
            .call
            .id
            .as_ref()
            .and_then(|id| by_id.remove(id))
            .or_else(|| positional.next());
        if let Some(replacement) = replacement {
            slot.output = replacement.output;
            slot.content = replacement.content;
            slot.switch_agent = replacement.switch_agent;
        }
    }

    // Externalize inline image data URIs in tool-result content to cid refs
    // before persisting, freeing the in-memory base64 when an attachment store
    // is configured;
    // the cid -> filename map is logged as a DataUrls entry after the
    // ToolResults entry (below) so the ToolCalls/ToolResults pairing on replay
    // is not split.
    externalize_tool_result_content(
        attachments_dir.as_deref(),
        &mut pending.tool_results,
        &mut cid_urls,
    );

    let log_results: Vec<harnx_core::session::ToolOutput> = pending
        .tool_results
        .iter()
        .map(|r| harnx_core::session::ToolOutput {
            id: r.call.id.clone(),
            name: r.call.name.clone(),
            output: r.output.clone(),
            markdown: r.markdown.clone(),
            content: r.content.clone(),
            switch_agent: r.switch_agent.clone(),
        })
        .collect();

    let appended = append_event(
        session,
        &SessionLogEntry::ToolResults {
            results: log_results,
            timestamp: Some(Utc::now()),
        },
    );
    let all_appended = appended & record_externalized(session, cid_urls);
    session.dirty |= !all_appended;
    require_authoritative_appends(session, all_appended, "tool results")?;
    persist_tool_execution_contexts(session, results, all_appended);
    session.update_tokens();
    Ok(())
}

fn persist_tool_execution_contexts(
    session: &Session,
    results: &[crate::tool::ToolResult],
    tool_results_are_durable: bool,
) {
    if !tool_results_are_durable {
        return;
    }
    let observations = results
        .iter()
        .filter_map(|result| result.execution_context.clone())
        .collect::<Vec<_>>();
    if observations.is_empty() {
        return;
    }
    if let Err(error) = persist_execution_contexts(session, &observations) {
        log::warn!(
            "failed to persist tool-observed execution context: session_id={} error={error:#}",
            session.id()
        );
    }
}

/// Returns `true` when `input` is a genuine tool-call continuation of
/// `session` — i.e. the session's last message is a `Tool` result AND
/// the input carries accumulated tool-call results from `merge_tool_results`.
///
/// Used in both `begin_turn` (persistence) and `build_messages` (wire
/// format) so that future edits to the heuristic only need to happen
/// in one place.  Fixes #390: without the `tool_calls.is_some()` guard,
/// a fresh user prompt arriving after an interrupted/resumed session that
/// ended with a `Tool` message was silently dropped.
fn is_tool_continuation(input: &Input, messages: &[Message]) -> bool {
    input.tool_calls.is_some() && messages.last().is_some_and(|m| m.role == MessageRole::Tool)
}

fn begin_turn(session: &mut Session, input: &Input, _output: &str) -> Result<bool> {
    let mut all_appended = true;
    let is_continuation = is_tool_continuation(input, &session.messages);

    if session.messages.is_empty() {
        all_appended &= append_initial_agent_messages(session, input)?;
    } else if !is_continuation && !input.skip_user_log_append {
        // `skip_user_log_append` is set by the NATS HA worker: the user
        // message is already durably logged by the client and present in the
        // loaded `session.messages`, so re-appending it would duplicate it and
        // reorder the assistant barrier past concurrent arrivals.
        all_appended &= append_user_turn_message(session, input);
    }

    all_appended &= append_input_data_urls(session, input);
    all_appended &= append_injected_user_text(session, input);
    Ok(all_appended)
}

fn append_initial_agent_messages(session: &mut Session, input: &Input) -> Result<bool> {
    let agent_messages = input.agent().build_messages(input)?;
    let mut all_appended = true;
    let mut cid_urls = std::collections::HashMap::new();

    for mut msg in agent_messages {
        if msg.role == MessageRole::System {
            continue;
        }
        // `externalize_content` only writes files + rewrites `content` in
        // memory (takes `&Session`, cannot append), so `seq` stays correct.
        let seq = session.next_seq();
        cid_urls.extend(externalize_content(session, &mut msg.content));
        all_appended &= append_session_message(session, msg.role, msg.content.clone());
        session.messages.push(msg.with_log_seq(seq));
    }

    // Seq-less metadata record for the cids written above; appended after
    // the messages so message log_seqs are unaffected.
    all_appended &= record_externalized(session, cid_urls);
    Ok(all_appended)
}

fn append_user_turn_message(session: &mut Session, input: &Input) -> bool {
    // `seq` is the message's log-document index. `externalize_content` only
    // writes attachment files + rewrites `content` in memory (it takes
    // `&Session`, so it cannot append), so nothing must be appended between
    // here and the Message append below or `seq` would be off by one. The
    // cid DataUrls entry is a seq-less metadata record appended *after* the
    // message.
    let seq = session.next_seq();
    let mut content = input.message_content();
    let cid_urls = externalize_content(session, &mut content);
    let user_msg = Message::new(MessageRole::User, content).with_log_seq(seq);
    let mut all_appended = append_session_message(session, user_msg.role, user_msg.content.clone());
    session.messages.push(user_msg);
    emit_agent_event(AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }));
    all_appended &= record_externalized(session, cid_urls);
    all_appended
}

fn append_input_data_urls(session: &mut Session, input: &Input) -> bool {
    let new_data_urls = input.data_urls();
    let mut all_appended = true;
    if !new_data_urls.is_empty() {
        all_appended &= append_event(
            session,
            &SessionLogEntry::DataUrls {
                urls: new_data_urls.clone(),
            },
        );
    }
    session.data_urls.extend(new_data_urls);
    all_appended
}

fn append_injected_user_text(session: &mut Session, input: &Input) -> bool {
    let Some(injected) = input.injected_user_text() else {
        return true;
    };

    // `skip_user_log_append` marks an input whose user text was folded out of
    // the durable log, so the log already holds this message and re-appending
    // it is not just a duplicate row: the worker's mid-round fold would read
    // the fresh copy as another unanswered message and inject it again, once
    // more per remaining tool round. The in-memory push still has to happen —
    // `session.messages` is loaded once per turn and is what carries the
    // injected message into the following rounds' wire requests.
    if input.skip_user_log_append {
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Text(injected.to_string()),
        ));
        return true;
    }

    let seq = session.next_seq();
    let injected_msg = Message::new(
        MessageRole::User,
        MessageContent::Text(injected.to_string()),
    )
    .with_log_seq(seq);
    let all_appended =
        append_session_message(session, injected_msg.role, injected_msg.content.clone());
    session.messages.push(injected_msg);
    emit_agent_event(AgentEvent::Session(SessionEvent::LogSeqAssigned { seq }));
    all_appended
}

fn append_session_message(
    session: &mut Session,
    role: MessageRole,
    content: MessageContent,
) -> bool {
    append_event(
        session,
        &SessionLogEntry::Message {
            id: Some(persisted_message_id()),
            role,
            content,
            timestamp: Some(Utc::now()),
            fence_token: None,
        },
    )
}

pub fn append_message(session: &mut Session, role: MessageRole, content: MessageContent) -> bool {
    append_session_message(session, role, content)
}

fn clear_messages_in_memory(session: &mut Session) {
    session.messages.clear();
    session.compressed_messages.clear();
    session.data_urls.clear();
    session.completion_usage = CompletionTokenUsage::default();
    session.update_tokens();
}

pub fn clear_messages(session: &mut Session) -> Result<()> {
    if !append_event(session, &SessionLogEntry::Clear) {
        session.dirty = true;
        bail!("Failed to persist session clear")
    }
    clear_messages_in_memory(session);
    Ok(())
}

pub(crate) fn clear_messages_after_persisted_clear(session: &mut Session) {
    clear_messages_in_memory(session);
}

pub fn echo_messages(session: &Session, input: &Input) -> String {
    let messages = build_messages(session, input).unwrap_or_default();
    serde_yaml::to_string(&messages).unwrap_or_else(|_| "Unable to echo message".into())
}

/// Build the outgoing message list and expand any `cid:` attachment references
/// back into inline `data:` URIs for transmission. Expansion is transient —
/// it affects only the returned messages, never the stored session.
pub fn build_messages(session: &Session, input: &Input) -> Result<Vec<Message>> {
    let messages = build_messages_inner(session, input)?;
    Ok(messages)
}

fn expand_message(
    encoder: &dyn crate::config::attachments::AttachmentEncoder,
    dir: &std::path::Path,
    content: &mut MessageContent,
) {
    let result = match content {
        MessageContent::Array(parts) => {
            crate::config::attachments::expand_parts(encoder, dir, parts)
        }
        MessageContent::ToolCalls(tool_calls) => {
            for tool_result in &mut tool_calls.tool_results {
                if let Err(error) =
                    crate::config::attachments::expand_parts(encoder, dir, &mut tool_result.content)
                {
                    log::warn!("attachment expansion failed: {error}");
                }
            }
            Ok(())
        }
        MessageContent::Text(_) => Ok(()),
    };
    if let Err(error) = result {
        log::warn!("attachment expansion failed: {error}");
    }
}

pub(crate) fn expand_message_attachments(session: &Session, messages: &mut [Message]) {
    let Some(dir) = super::session_externalize::attachments_dir(session) else {
        return;
    };
    let encoder = crate::config::attachments::Base64Encoder;
    for message in messages {
        expand_message(&encoder, &dir, &mut message.content);
    }
}

/// Drop the trailing assistant/tool messages so a regenerate re-answers from
/// the last user message.
fn trim_trailing_non_user(messages: &mut Vec<Message>) {
    while messages.last().is_some_and(|last| !last.role.is_user()) {
        messages.pop();
    }
}

/// Whether the trailing `input.message_content()` user message must be
/// suppressed because the history already carries that text.
fn history_already_has_input_text(input: &Input, messages: &[Message]) -> bool {
    // Mid-tool-round: the pending call/result pair is already in the history.
    is_tool_continuation(input, messages)
        // The NATS worker folds its user text out of the durable log, so the
        // session loaded for this turn already ends with those messages.
        // Pushing them again would send the prompt to the model twice — once
        // per turn, since every worker turn derives its input the same way.
        || input.skip_user_log_append
}

/// Replace any persisted leading system message with a freshly rendered one, so
/// each turn sees current agent variables, resolved tools, and the active model
/// selection (including fallbacks). Agent swaps after construction (e.g.
/// compaction) also flow through here.
fn inject_fresh_system_prompt(messages: &mut Vec<Message>, input: &Input) -> Result<()> {
    if !input.inject_system_prompt() {
        return Ok(());
    }
    let system_text = input
        .agent()
        .system_text_with_tools(input.resolved_tools.as_deref().unwrap_or_default())?;
    // Drop leading system message(s) so only the freshly rendered prompt
    // survives — including when a legacy transcript stored one but the current
    // render is empty.
    while matches!(messages.first().map(|m| m.role), Some(MessageRole::System)) {
        messages.remove(0);
    }
    if !system_text.is_empty() {
        messages.insert(
            0,
            Message::new(MessageRole::System, MessageContent::Text(system_text)),
        );
    }
    Ok(())
}

/// Prepend the tail of the compressed transcript to a single-message history
/// from its last user message on, so a compacted session keeps recent context
/// in chronological order.
fn prepend_compressed_tail(session: &Session, messages: &mut Vec<Message>) {
    if messages.len() != 1 || session.compressed_messages.len() < 2 {
        return;
    }
    if let Some(index) = session
        .compressed_messages
        .iter()
        .rposition(|v| v.role == MessageRole::User)
    {
        messages.splice(0..0, session.compressed_messages[index..].iter().cloned());
    }
}

fn build_messages_inner(session: &Session, input: &Input) -> Result<Vec<Message>> {
    let mut messages = session.messages.clone();
    if input.continue_output().is_some() {
        return Ok(messages);
    }
    if input.regenerate() {
        trim_trailing_non_user(&mut messages);
        return Ok(messages);
    }
    let need_add_msg = if messages.is_empty() {
        messages = input.agent().build_messages(input)?;
        false
    } else {
        prepend_compressed_tail(session, &mut messages);
        !history_already_has_input_text(input, &messages)
    };
    inject_fresh_system_prompt(&mut messages, input)?;
    if need_add_msg {
        messages.push(Message::new(MessageRole::User, input.message_content()));
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingAuthoritativeSink;

    impl SessionAppendSink for FailingAuthoritativeSink {
        fn append(&self, _entry: &SessionLogEntry) -> Result<u64> {
            anyhow::bail!("simulated durable append failure")
        }

        fn persist_title(&self, _title: &str, _manual: bool, _tokens: usize) -> Result<()> {
            anyhow::bail!("simulated durable metadata failure")
        }

        fn failure_is_fatal(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct FailingContextMetadataSink {
        next_seq: std::sync::atomic::AtomicU64,
        context_writes: std::sync::atomic::AtomicUsize,
    }

    impl SessionAppendSink for FailingContextMetadataSink {
        fn append(&self, _entry: &SessionLogEntry) -> Result<u64> {
            Ok(self
                .next_seq
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1)
        }

        fn persist_execution_contexts(
            &self,
            _observations: &[harnx_core::execution_context::ExecutionContextObservation],
        ) -> Result<()> {
            self.context_writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("simulated execution-context metadata failure")
        }

        fn failure_is_fatal(&self) -> bool {
            true
        }
    }

    fn test_session() -> Session {
        let mut session = new(&Config::default(), "test", None).unwrap();
        attach_memory_log(&mut session);
        session
    }

    fn message_view(messages: &[Message]) -> Vec<(MessageRole, String)> {
        messages
            .iter()
            .map(|m| (m.role, m.content.to_text()))
            .collect()
    }

    #[test]
    fn authoritative_assistant_append_failure_fails_the_turn() {
        let config = Config::default();
        let global_config = Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(
            &global_config,
            "question",
            Some(config.extract_agent()),
        );
        let mut session = new(&config, "authoritative-failure", None).unwrap();
        session.runtime = Some(Arc::new(
            Arc::new(FailingAuthoritativeSink) as Arc<dyn SessionAppendSink>
        ));

        let error = add_assistant_text(&mut session, &input, "answer", None)
            .expect_err("authoritative persistence failure must fail the turn");

        assert!(error
            .to_string()
            .contains("failed to durably persist assistant response"));
        assert!(session.dirty);
    }

    #[test]
    fn context_metadata_failure_does_not_fail_durable_tool_result() {
        let config = Config::default();
        let global_config = Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(
            &global_config,
            "inspect repository",
            Some(config.extract_agent()),
        );
        let sink = Arc::new(FailingContextMetadataSink::default());
        let mut session = new(&config, "context-metadata-failure", None).unwrap();
        session.runtime = Some(Arc::new(sink.clone() as Arc<dyn SessionAppendSink>));
        let call = harnx_core::tool::ToolCall {
            name: "fs_read".to_string(),
            arguments: json!({"path": "/workspace/README.md"}),
            id: Some("call-1".to_string()),
            thought_signature: None,
        };
        super::add_tool_calls(
            &mut session,
            &input,
            "reading",
            None,
            std::slice::from_ref(&call),
        )
        .unwrap();
        let mut result = ToolResult::new(call, json!({"ok": true}));
        let mut observation = harnx_core::execution_context::ExecutionContextObservation::observe(
            std::path::Path::new("/workspace"),
            std::path::Path::new("/workspace/README.md"),
        );
        observation.provenance = Some(
            harnx_core::execution_context::ToolObservationProvenance::new(
                "scope", "fs", "read", "call-1",
            ),
        );
        result.execution_context = Some(observation);

        super::add_tool_results(&mut session, &[result])
            .expect("durable result must survive context metadata failure");
        assert_eq!(
            sink.context_writes
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn authoritative_title_append_failure_does_not_publish_an_in_memory_title() {
        let mut session = new(&Config::default(), "title-failure", None).unwrap();
        session.runtime = Some(Arc::new(
            Arc::new(FailingAuthoritativeSink) as Arc<dyn SessionAppendSink>
        ));

        let error = record_title(&mut session, "Unpersisted title".to_string(), false, 42)
            .expect_err("authoritative title persistence failure must be visible");

        assert!(error.to_string().contains("durably persist session title"));
        assert_eq!(session.title(), None);
        assert_eq!(session.title_last_updated_tokens(), 0);
    }

    #[test]
    fn canonical_nats_replay_rejects_legacy_header_before_mutation_resolution() {
        let raw_entries = collect_raw_log_entries(
            "---\ntype: header\nmodel_id: legacy-model\n---\ntype: message\nrole: user\ncontent: hello\n",
            "legacy-session",
        )
        .expect("legacy YAML remains syntactically readable");
        assert!(matches!(raw_entries[0].1, SessionLogEntry::Unknown));

        let error =
            replay_nats_entries_into_session(&raw_entries, "legacy-session", Session::default())
                .expect_err("canonical NATS replay must reject embedded legacy metadata");

        assert!(error.to_string().contains("unsupported or legacy entry"));
    }

    #[test]
    fn load_from_log_enumerates_document_sequence_numbers() {
        let content = r#"---
type: message
role: user
content: first
---
type: rewind
after_seq: 1
---
type: edit_entries
from: 1
to: 1
replacements: []
---
type: message
role: assistant
content: second
"#;

        let seqs: Vec<_> = serde_yaml::Deserializer::from_str(content)
            .enumerate()
            .map(|(seq, document)| {
                let entry = SessionLogEntry::deserialize(document).expect("valid entry");
                (seq, entry)
            })
            .collect();

        assert_eq!(seqs.len(), 4);
        assert!(matches!(
            seqs[0],
            (
                0,
                SessionLogEntry::Message {
                    timestamp: None,
                    ..
                }
            )
        ));
        assert!(matches!(
            seqs[1],
            (1, SessionLogEntry::Rewind { after_seq: 1 })
        ));
        assert!(matches!(
            seqs[2],
            (2, SessionLogEntry::EditEntries { from: 1, to: 1, .. })
        ));
        assert!(matches!(
            seqs[3],
            (
                3,
                SessionLogEntry::Message {
                    timestamp: None,
                    ..
                }
            )
        ));

        let session = super::load_from_log_for_test(content);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content.to_text(), "first");
        assert_eq!(session.messages[1].content.to_text(), "second");
        assert_eq!(session.log_entry_count, 4);
        assert_eq!(session.next_seq(), 4);
    }

    #[test]
    fn compress_hard_cut_layout_preserves_recent_suffix_on_reload() {
        // Hard-cut log layout: header + prefix message entries + a compress
        // entry (archives the prefix, carries the summary) + the preserved
        // suffix re-logged as fresh message entries after it. Replay must
        // archive the prefix into compressed_messages, push the summary system
        // message, then replay the repeated suffix entries as live messages —
        // no stored index required.
        let content = r#"---
type: header
model: openai:gpt-4o
---
type: message
role: system
content: old system prompt
---
type: message
role: user
content: prefix question
---
type: message
role: assistant
content: prefix answer
---
type: compress
prompt: summary of earlier conversation
---
type: message
role: user
content: recent question
---
type: message
role: assistant
content: recent answer
"#;

        let session = super::load_from_log_for_test(content);

        // Live messages: kept suffix only; summary stays on runtime field.
        assert_eq!(
            message_view(&session.messages),
            vec![
                (MessageRole::User, "recent question".to_string()),
                (MessageRole::Assistant, "recent answer".to_string()),
            ]
        );
        assert_eq!(
            session.compaction_summary.as_deref(),
            Some("summary of earlier conversation")
        );

        // The prefix entries before the compress event must have been archived.
        assert_eq!(
            message_view(&session.compressed_messages),
            vec![
                (MessageRole::System, "old system prompt".to_string()),
                (MessageRole::User, "prefix question".to_string()),
                (MessageRole::Assistant, "prefix answer".to_string()),
            ]
        );
    }

    #[test]
    fn compress_bare_moves_all_messages_on_reload() {
        // Legacy/move-all layout (and the full `compress`): a bare compress
        // entry with no suffix re-logged after it archives every preceding
        // message; only the summary survives.
        let content = r#"---
type: header
model: openai:gpt-4o
---
type: message
role: user
content: first
---
type: message
role: assistant
content: second
---
type: compress
prompt: summary
"#;

        let session = super::load_from_log_for_test(content);

        assert!(session.messages.is_empty());
        assert_eq!(session.compaction_summary.as_deref(), Some("summary"));
        assert_eq!(
            message_view(&session.compressed_messages),
            vec![
                (MessageRole::User, "first".to_string()),
                (MessageRole::Assistant, "second".to_string()),
            ]
        );
    }

    #[test]
    fn set_agent_to_agent_round_trip_preserves_model_fallbacks() {
        let agent = Agent::new(AgentConfig::from_markdown(
            "test",
            "---\nmodel: openai:gpt-4o\nmodel_fallbacks:\n  - anthropic:claude\n  - google:gemini\n---\nYou are a test agent.",
        ).unwrap());
        let mut session = test_session();

        session.set_agent(&agent).unwrap();
        let round_tripped_agent = to_agent(&session);

        assert_eq!(
            round_tripped_agent.model_fallbacks(),
            agent.model_fallbacks()
        );
    }

    #[test]
    fn export_shows_model_fallbacks() {
        let mut session = test_session();
        session.set_model_fallbacks(vec![
            "anthropic:claude".to_string(),
            "google:gemini".to_string(),
        ]);

        let output = session.export().unwrap();

        assert!(output.contains("model_fallbacks:"));
        assert!(output.contains("- anthropic:claude"));
        assert!(output.contains("- google:gemini"));
    }

    /// Regression test for #390: a fresh user message sent after a
    /// session that ended with a Tool message (e.g. Ctrl-C mid-round,
    /// then resume) must NOT be dropped.
    ///
    /// Old code: `build_messages` checked only `last.role == Tool` →
    /// treated a fresh prompt as a continuation and suppressed the
    /// user message entirely.
    ///
    /// Fixed: also requires `input.tool_calls.is_some()`.
    #[test]
    fn fresh_message_after_tool_tail_is_included_in_build_messages() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let _tmp = TempDir::new().unwrap();
        let mut session = test_session();

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));

        // Build a session that ends with a Tool message (simulates
        // an interrupted session or one resumed after Ctrl-C).
        let input1 =
            crate::config::input::from_str(&global_config, "original query", Some(agent.clone()));
        let call = ToolCall {
            name: "Bash".to_string(),
            arguments: json!({"command": "ls"}),
            id: Some("c1".to_string()),
            thought_signature: None,
        };
        let result = ToolResult::new(call.clone(), json!({"stdout": "file1\n"}));
        super::add_tool_calls(&mut session, &input1, "running bash", None, &[call]).unwrap();
        super::add_tool_results(&mut session, &[result]).unwrap();
        assert_eq!(
            session.messages.last().unwrap().role,
            MessageRole::Tool,
            "session tail must be Tool to exercise the regression"
        );

        // Now simulate a fresh user message (no tool_calls on the input —
        // this is the post-interrupt / post-resume scenario from #390).
        let fresh_input = crate::config::input::from_str(
            &global_config,
            "new message after interrupt",
            Some(agent),
        );
        assert!(
            fresh_input.tool_calls.is_none(),
            "fresh input must have no tool_calls"
        );

        let messages = super::build_messages(&session, &fresh_input).unwrap();

        // The fresh user message must appear in the built message list.
        let user_messages: Vec<_> = messages.iter().filter(|m| m.role.is_user()).collect();
        assert!(
            user_messages
                .iter()
                .any(|m| m.content.to_text().contains("new message after interrupt")),
            "fresh user message must be included; got messages: {messages:#?}"
        );
    }

    /// Regression test for #390 (persistence side): `begin_turn` must
    /// persist a fresh user message even when the session tail is Tool.
    #[test]
    fn fresh_message_after_tool_tail_is_saved_by_begin_turn() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let _tmp = TempDir::new().unwrap();
        let mut session = test_session();

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));

        // Build a session ending in a Tool message.
        let input1 =
            crate::config::input::from_str(&global_config, "original query", Some(agent.clone()));
        let call = ToolCall {
            name: "Bash".to_string(),
            arguments: json!({"command": "ls"}),
            id: Some("c2".to_string()),
            thought_signature: None,
        };
        let result = ToolResult::new(call.clone(), json!({"stdout": "file1\n"}));
        super::add_tool_calls(&mut session, &input1, "running bash", None, &[call]).unwrap();
        super::add_tool_results(&mut session, &[result]).unwrap();

        // Fresh message (no tool_calls) — `add_assistant_text` calls
        // `begin_turn` internally.
        let fresh_input =
            crate::config::input::from_str(&global_config, "follow-up after resume", Some(agent));
        super::add_assistant_text(&mut session, &fresh_input, "here is my reply", None).unwrap();

        // The follow-up user message must have been saved to the session.
        let user_messages: Vec<_> = session
            .messages
            .iter()
            .filter(|m| m.role.is_user())
            .collect();
        assert!(
            user_messages
                .iter()
                .any(|m| m.content.to_text().contains("follow-up after resume")),
            "fresh user message must be persisted; messages: {:#?}",
            session.messages
        );
    }

    #[test]
    fn load_from_log_accepts_old_message_entries_without_ids() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("old-session.yaml");
        let content = r#"---
type: header
model: openai:gpt-4o
---
type: message
role: user
content: legacy prompt
---
type: message
role: assistant
content: legacy reply
"#;
        std::fs::write(&path, content).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded = load_from_log_for_test(&content);

        assert_eq!(
            loaded.messages.len(),
            2,
            "both legacy messages must reconstruct"
        );
        assert!(
            loaded.messages.iter().all(|message| message.id.is_none()),
            "pre-P1.5 messages without id fields must reconstruct with id == None"
        );
        assert_eq!(
            loaded.messages[0].log_seq,
            Some(1),
            "legacy user message should still get seq fallback coordinate"
        );
        assert_eq!(
            loaded.messages[1].log_seq,
            Some(2),
            "legacy assistant message should still get seq fallback coordinate"
        );
        assert_eq!(loaded.messages[0].content.to_text(), "legacy prompt");
        assert_eq!(loaded.messages[1].content.to_text(), "legacy reply");
    }

    #[test]
    fn replay_skips_orphan_tool_results_and_preserves_the_rest_of_the_transcript() {
        let content = r#"---
type: header
model: openai:gpt-4o
---
type: message
role: user
content: before corruption
---
type: tool_results
results:
  - id: missing-call
    name: time
    output: ignored orphan result
---
type: message
role: assistant
content: recovered reply
"#;

        let loaded = load_from_log_for_test(content);

        assert_eq!(
            message_view(&loaded.messages),
            vec![
                (MessageRole::User, "before corruption".to_string()),
                (MessageRole::Assistant, "recovered reply".to_string()),
            ]
        );
        assert_eq!(loaded.replay_warnings.len(), 1);
        assert!(loaded.replay_warnings[0].contains("orphan tool results"));
        assert!(loaded.replay_warnings[0].contains("log entry 2"));
    }

    #[test]
    fn replay_pairs_tool_results_across_a_queued_user_message() {
        let content = r#"---
type: message
role: user
content: original request
---
type: tool_calls
text: working
calls:
  - name: search
    arguments: {}
    id: call-1
---
type: message
role: user
content: queued correction
---
type: tool_results
results:
  - id: call-1
    name: search
    output:
      answer: found
"#;

        let loaded = load_from_log_for_test(content);

        assert_eq!(
            loaded
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            vec![MessageRole::User, MessageRole::Tool, MessageRole::User],
            "the tool result must precede input queued while that tool was running"
        );
        let MessageContent::ToolCalls(tool_calls) = &loaded.messages[1].content else {
            panic!("expected reconstructed tool message");
        };
        assert_eq!(
            tool_calls.tool_results[0].output,
            serde_json::json!({"answer": "found"})
        );
        assert!(loaded.replay_warnings.is_empty());
    }

    #[test]
    fn replay_ignores_duplicate_results_for_a_completed_tool_call() {
        let content = r#"---
type: message
role: user
content: original request
---
type: tool_calls
text: working
calls:
  - name: search
    arguments: {}
    id: call-1
---
type: message
role: user
content: queued correction
---
type: tool_results
results:
  - id: call-1
    name: search
    output:
      error: tool response lost
---
type: turn_end
through_seq: 3
fence_token: 7
---
type: tool_results
results:
  - id: call-1
    name: search
    output:
      error: tool response lost
---
type: turn_end
through_seq: 3
fence_token: 8
---
type: tool_results
results:
  - id: call-1
    name: search
    output:
      error: tool response lost
"#;

        let loaded = load_from_log_for_test(content);

        assert_eq!(
            loaded
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            vec![MessageRole::User, MessageRole::Tool, MessageRole::User]
        );
        assert!(loaded.replay_warnings.is_empty());
    }

    #[test]
    fn leased_observer_replay_preserves_a_trailing_tool_call_as_pending() {
        let content = r#"---
type: message
role: user
content: original request
---
type: tool_calls
text: still working
calls:
  - name: search
    arguments: {}
    id: call-pending
---
type: message
role: user
content: queued correction
"#;
        let raw = collect_raw_log_entries(content, "leased-session").unwrap();

        let loaded = replay_nats_entries_into_session_preserving_pending(
            &raw,
            "leased-session",
            Session::default(),
        )
        .unwrap();

        assert_eq!(
            loaded
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            vec![MessageRole::User, MessageRole::Tool, MessageRole::User]
        );
        let MessageContent::ToolCalls(tool_calls) = &loaded.messages[1].content else {
            panic!("expected pending tool-call message");
        };
        assert_eq!(tool_calls.tool_results.len(), 1);
        assert!(is_pending_tool_result(&tool_calls.tool_results[0]));
        assert!(loaded.replay_warnings.is_empty());
    }

    #[test]
    fn legacy_tool_result_without_content_loads_with_empty_content() {
        use serde_json::json;

        let content = r#"---
type: header
model: test:model
agent: default
---
type: user
text: find test
---
type: tool_calls
text: searching...
calls:
  - name: search
    arguments:
      query: test
    id: c1
---
type: tool_results
results:
  - id: c1
    name: search
    output:
      results:
        - a
        - b
"#;

        let reloaded = super::load_from_log_for_test(content);
        let tool_msg = reloaded
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("session should contain a Tool message");
        let MessageContent::ToolCalls(tc) = &tool_msg.content else {
            panic!("expected ToolCalls content on Tool message");
        };

        assert_eq!(tc.tool_results[0].output, json!({"results": ["a", "b"]}));
        assert!(
            tc.tool_results[0].content.is_empty(),
            "legacy sessions without content field should load as empty Vec"
        );
    }

    #[test]
    fn load_old_tool_results_without_content_defaults_to_empty_content() {
        let content = r#"type: header
model: test
---
type: message
role: user
content: inspect image
---
type: tool_calls
text: reading...
calls:
  - name: read
    arguments:
      path: chart.png
    id: c1
---
type: tool_results
results:
  - id: c1
    name: read
    output:
      content:
        - type: image
          mime_type: image/png
          data: "<image: image/png, 4 base64 chars>"
"#;

        let session = super::load_from_log_for_test(content);
        let MessageContent::ToolCalls(tool_calls) = &session
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("tool message should load")
            .content
        else {
            panic!("expected tool-call message");
        };
        assert_eq!(tool_calls.tool_results.len(), 1);
        assert!(tool_calls.tool_results[0].content.is_empty());
    }

    #[test]
    fn load_replays_rewind_entries() {
        let content = r#"type: header
model: test
---
type: message
role: user
content: zero
---
type: message
role: assistant
content: one
---
type: message
role: user
content: two
---
type: message
role: assistant
content: three
---
type: message
role: user
content: four
---
type: rewind
after_seq: 2
"#;

        let session = super::load_from_log_for_test(content);
        let texts: Vec<_> = session
            .messages
            .iter()
            .map(|m| m.content.to_text())
            .collect();

        assert_eq!(texts, vec!["zero", "one"]);
    }

    #[test]
    fn load_replays_edit_entries_replace() {
        let content = r#"type: header
model: test
---
type: message
role: user
content: before
---
type: message
role: assistant
content: replace me
---
type: message
role: user
content: after
---
type: edit_entries
from: 2
to: 2
replacements:
  - |
    type: message
    role: assistant
    content: replaced
"#;

        let session = super::load_from_log_for_test(content);
        let texts: Vec<_> = session
            .messages
            .iter()
            .map(|m| m.content.to_text())
            .collect();

        assert_eq!(texts, vec!["before", "replaced", "after"]);
    }

    #[test]
    fn load_replays_edit_entries_delete() {
        let content = r#"type: header
model: test
---
type: message
role: user
content: keep one
---
type: message
role: assistant
content: delete me
---
type: message
role: user
content: keep two
---
type: edit_entries
from: 2
to: 2
replacements: []
"#;

        let session = super::load_from_log_for_test(content);
        let texts: Vec<_> = session
            .messages
            .iter()
            .map(|m| m.content.to_text())
            .collect();

        assert_eq!(texts, vec!["keep one", "keep two"]);
    }

    #[test]
    fn load_replays_stacked_mutations_edit_then_rewind() {
        let content = r#"type: header
model: test
---
type: message
role: user
content: zero
---
type: message
role: assistant
content: one
---
type: message
role: user
content: two
---
type: edit_entries
from: 2
to: 2
replacements:
  - |
    type: message
    role: assistant
    content: one edited
---
type: rewind
after_seq: 3
"#;

        let session = super::load_from_log_for_test(content);
        let texts: Vec<_> = session
            .messages
            .iter()
            .map(|m| m.content.to_text())
            .collect();

        assert_eq!(texts, vec!["zero", "two"]);
    }

    #[test]
    fn load_replays_stacked_mutations_rewind_then_edit() {
        let content = r#"type: header
model: test
---
type: message
role: user
content: zero
---
type: message
role: assistant
content: one
---
type: message
role: user
content: two
---
type: message
role: assistant
content: three
---
type: rewind
after_seq: 2
---
type: edit_entries
from: 1
to: 1
replacements:
  - |
    type: message
    role: user
    content: zero edited
"#;

        let session = super::load_from_log_for_test(content);
        let texts: Vec<_> = session
            .messages
            .iter()
            .map(|m| m.content.to_text())
            .collect();

        assert_eq!(texts, vec!["zero edited", "one"]);
    }

    #[test]
    fn load_replays_stacked_mutations_edit_one_to_many_then_edit_later_entry() {
        let content = r#"type: header
model: test
---
type: message
role: user
content: original 1
---
type: message
role: assistant
content: original 2
---
type: edit_entries
from: 1
to: 1
replacements:
  - |
    type: message
    role: user
    content: 1a
  - |
    type: message
    role: assistant
    content: 1b
---
type: edit_entries
from: 2
to: 2
replacements:
  - |
    type: message
    role: assistant
    content: 2x
"#;

        let session = super::load_from_log_for_test(content);
        let texts: Vec<_> = session
            .messages
            .iter()
            .map(|m| m.content.to_text())
            .collect();

        assert_eq!(texts, vec!["1a", "1b", "2x"]);
    }

    #[test]
    fn load_replays_stacked_mutations_reedit_expanded_mutation_seq() {
        let content = r#"type: header
model: test
---
type: message
role: user
content: msg1
---
type: message
role: assistant
content: msg2
---
type: edit_entries
from: 1
to: 1
replacements:
  - |
    type: message
    role: user
    content: expanded_a
  - |
    type: message
    role: assistant
    content: expanded_b
---
type: edit_entries
from: 3
to: 3
replacements:
  - |
    type: message
    role: user
    content: re-edited
"#;

        let session = super::load_from_log_for_test(content);
        let texts: Vec<_> = session
            .messages
            .iter()
            .map(|m| m.content.to_text())
            .collect();

        assert_eq!(texts, vec!["re-edited", "msg2"]);
    }

    #[test]
    fn render_shows_session_title_and_manual_state() {
        let mut session = test_session();

        let output = super::render(&session).unwrap();
        assert!(output.contains("title               (none)"), "{output}");

        session.set_title("User chosen title".to_string());
        session.set_title_last_updated_tokens(usize::MAX);
        let output = super::render(&session).unwrap();
        assert!(
            output.contains("title               User chosen title (manual)"),
            "{output}"
        );
    }

    #[test]
    fn render_shows_model_fallbacks() {
        let mut session = test_session();
        session.set_model_fallbacks(vec![
            "anthropic:claude".to_string(),
            "google:gemini".to_string(),
        ]);

        let output = super::render(&session).unwrap();

        assert!(
            output.contains("model_fallbacks"),
            "render output should contain model_fallbacks key: {output}"
        );
        assert!(
            output.contains("anthropic:claude,google:gemini"),
            "render output should contain comma-separated fallback values: {output}"
        );
    }

    #[test]
    fn render_shows_turns_count_for_user_messages() {
        use harnx_core::message::MessageRole;

        let mut session = test_session();
        session.push_message_for_test(MessageRole::User, "hello".to_string());
        session.push_message_for_test(MessageRole::Assistant, "hi there".to_string());
        session.push_message_for_test(MessageRole::User, "thanks".to_string());
        session.update_tokens();

        let output = super::render(&session).unwrap();

        assert!(
            output.contains("turns               2"),
            "render output should show 2 user turns: {output}"
        );
        assert!(
            output.contains("tokens"),
            "render output should show tokens line: {output}"
        );
    }

    /// `begin_turn` writes `input.injected_user_text` to the session log on
    /// every call where the field is set — it does not clear the field. The
    /// agent loop is responsible for resetting `injected_user_text` between
    /// iterations; if it forgets, the same user message is appended on every
    /// tool round and the LLM sees N copies of one user message.
    #[test]
    fn injected_user_text_appended_once_per_begin_turn_call() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let _tmp = TempDir::new().unwrap();
        let mut session = test_session();

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));

        let mut input =
            crate::config::input::from_str(&global_config, "do work", Some(agent.clone()));
        input.set_injected_user_text("queued message".to_string());

        let call_a = ToolCall {
            name: "tool_a".to_string(),
            arguments: json!({}),
            id: Some("a1".to_string()),
            thought_signature: None,
        };
        super::add_tool_calls(
            &mut session,
            &input,
            "round 1",
            None,
            std::slice::from_ref(&call_a),
        )
        .unwrap();
        super::add_tool_results(
            &mut session,
            &[ToolResult::new(call_a, json!({"ok": true}))],
        )
        .unwrap();

        // Without the agent_loop clearing `injected_user_text` between rounds,
        // the SAME `input` reused for round 2 reapplies the injection.
        let call_b = ToolCall {
            name: "tool_b".to_string(),
            arguments: json!({}),
            id: Some("b1".to_string()),
            thought_signature: None,
        };
        super::add_tool_calls(
            &mut session,
            &input,
            "round 2",
            None,
            std::slice::from_ref(&call_b),
        )
        .unwrap();
        super::add_tool_results(
            &mut session,
            &[ToolResult::new(call_b, json!({"ok": true}))],
        )
        .unwrap();

        let injected_count = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User && m.content.to_text() == "queued message")
            .count();
        assert_eq!(
            injected_count, 2,
            "begin_turn appends injected_user_text every call when the field stays set; \
             callers (the agent loop) must clear it between rounds to avoid duplicates"
        );

        // Mirror of the agent_loop fix: clearing the field between rounds
        // restores the desired one-copy-per-injection behavior.
        let mut input_cleared = input.clone();
        input_cleared.injected_user_text = None;
        let call_c = ToolCall {
            name: "tool_c".to_string(),
            arguments: json!({}),
            id: Some("c1".to_string()),
            thought_signature: None,
        };
        super::add_tool_calls(
            &mut session,
            &input_cleared,
            "round 3",
            None,
            std::slice::from_ref(&call_c),
        )
        .unwrap();
        super::add_tool_results(
            &mut session,
            &[ToolResult::new(call_c, json!({"ok": true}))],
        )
        .unwrap();

        let injected_count_after_clear = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User && m.content.to_text() == "queued message")
            .count();
        assert_eq!(
            injected_count_after_clear, 2,
            "after clearing injected_user_text, no further duplicates are appended"
        );
    }

    /// Regression test: during multi-round tool execution, the agent
    /// loop reuses the same `Input` per round. `session.messages`
    /// already contains the user's original query (saved by
    /// `begin_turn` on round 1), so `build_messages` must NOT append
    /// another copy of `input.message_content()` at the end. The
    /// continuation marker is the last in-memory message being a
    /// `Tool`-role pending tool round — same heuristic `begin_turn`
    /// uses to skip its own user-message push.
    ///
    /// Original symptom: every multi-round request ended with the
    /// user's original question appended after the tool_result, so the
    /// model treated each round as if the user had re-asked the same
    /// question and looped emitting "Let me look at the current state…"
    /// forever.
    #[test]
    fn build_messages_does_not_append_duplicate_user_during_tool_round() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let _tmp = TempDir::new().unwrap();
        let mut session = test_session();

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config));
        let user_text =
            "I noticed something in the agent prompts and want to look at it".to_string();
        let input = crate::config::input::from_str(&global_config, &user_text, Some(agent));

        // Round 1: save a tool round to the session as the agent loop would.
        let call = ToolCall {
            name: "Read".to_string(),
            arguments: json!({"path": "/tmp/x"}),
            id: Some("toolu_round1".to_string()),
            thought_signature: None,
        };
        super::add_tool_calls(
            &mut session,
            &input,
            "Let me look at the directory.",
            None,
            std::slice::from_ref(&call),
        )
        .unwrap();
        super::add_tool_results(
            &mut session,
            &[ToolResult::new(call, json!({"content": "file body"}))],
        )
        .unwrap();

        // session.messages now ends with a Tool message — that's the
        // signal that we're mid-tool-round. The next agent_loop iteration
        // calls `merge_tool_results` on the input to carry the tool-call
        // context, then calls `build_messages` with that merged input.
        assert_eq!(session.messages.last().unwrap().role, MessageRole::Tool);

        let result = ToolResult::new(
            ToolCall {
                name: "Read".to_string(),
                arguments: json!({"path": "/tmp/x"}),
                id: Some("toolu_round1".to_string()),
                thought_signature: None,
            },
            json!({"content": "file body"}),
        );
        let merged_input = input.merge_tool_results(
            "Let me look at the directory.".to_string(),
            None,
            vec![result],
        );

        let messages = super::build_messages(&session, &merged_input).unwrap();

        let user_text_count = messages
            .iter()
            .filter(|m| m.role == MessageRole::User && m.content.to_text() == user_text)
            .count();
        assert_eq!(
            user_text_count, 1,
            "user's original question should appear exactly once in the wire-format \
             request; appending it again after the tool round makes the model think \
             the user re-asked and loops on 'Let me look at the current state…'. \
             messages: {messages:#?}"
        );
    }

    #[test]
    fn compress_keeping_recent_preserves_suffix() {
        use harnx_core::message::{Message, MessageContent, MessageRole};
        let mut session = test_session();
        session.messages = vec![
            Message::new(MessageRole::System, MessageContent::Text("sys".into())),
            Message::new(MessageRole::User, MessageContent::Text("old u".into())),
            Message::new(MessageRole::Assistant, MessageContent::Text("old a".into())),
            Message::new(MessageRole::User, MessageContent::Text("recent u".into())),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("recent a".into()),
            ),
        ];
        super::compress_keeping_recent(&mut session, "SUMMARY".to_string(), 3);

        assert_eq!(session.compressed_messages.len(), 3);
        assert_eq!(session.compaction_summary.as_deref(), Some("SUMMARY"));
        assert_eq!(session.messages.len(), 2);
        assert!(session
            .messages
            .iter()
            .all(|message| message.role != MessageRole::System));
        assert_eq!(session.messages[0].content.to_text(), "recent u");
        assert_eq!(session.messages[1].content.to_text(), "recent a");
    }

    #[test]
    fn build_messages_prepends_compressed_tail_before_single_live_message() {
        let mut session = test_session();
        session.compressed_messages = vec![
            Message::new(
                MessageRole::User,
                MessageContent::Text("archived question".to_string()),
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("archived answer".to_string()),
            ),
        ];
        session.messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Text("live question".to_string()),
        )];

        let input = Input::new(
            "next question".to_string(),
            ("next question".to_string(), vec![]),
            harnx_core::agent_config::AgentConfig::from_prompt(""),
        );
        let messages = super::build_messages(&session, &input).unwrap();

        assert_eq!(
            message_view(&messages),
            vec![
                (MessageRole::User, "archived question".to_string()),
                (MessageRole::Assistant, "archived answer".to_string()),
                (MessageRole::User, "live question".to_string()),
                (MessageRole::User, "next question".to_string()),
            ]
        );
    }

    #[test]
    fn build_messages_reinjects_system_prompt_after_compaction() {
        let mut session = test_session();
        session.agent_instructions = "Agent prompt with {{ agent.model }}".to_string();
        session.model_id = "openai:gpt-4o-mini".to_string();
        session.model = harnx_core::model::Model::new("openai", "gpt-4o-mini");
        session.messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Text("recent question".to_string()),
        )];
        session.compaction_summary = Some("summary".to_string());

        let input = Input::new(
            "next question".to_string(),
            ("next question".to_string(), vec![]),
            to_agent(&session).into_config(),
        );
        let messages = super::build_messages(&session, &input).unwrap();

        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(
            messages[0].content.to_text(),
            "Agent prompt with openai:gpt-4o-mini"
        );
    }

    #[test]
    fn to_agent_prefers_raw_agent_instructions_over_stale_agent_prompt_on_resume() {
        let mut session = test_session();
        session.agent_instructions = "Model={{ agent.model }}".to_string();
        session.agent_prompt = "Model=openai:gpt-4o-mini".to_string();
        session.model_id = "openai:gpt-4o-mini".to_string();
        session.model = harnx_core::model::Model::new("openai", "gpt-4o-mini");

        let input = Input::new(
            "question".to_string(),
            ("question".to_string(), vec![]),
            to_agent(&session).into_config(),
        );
        let messages = super::build_messages(&session, &input).unwrap();
        assert_eq!(messages[0].content.to_text(), "Model=openai:gpt-4o-mini");

        session.model_id = "openai:gpt-4o".to_string();
        session.model = harnx_core::model::Model::new("openai", "gpt-4o");
        let input = Input::new(
            "question".to_string(),
            ("question".to_string(), vec![]),
            to_agent(&session).into_config(),
        );
        let messages = super::build_messages(&session, &input).unwrap();
        assert_eq!(messages[0].content.to_text(), "Model=openai:gpt-4o");
    }

    #[test]
    fn build_messages_replaces_legacy_stored_system_prompt_with_fresh_render() {
        let mut session = test_session();
        session.agent_instructions = String::new();
        session.agent_name = Some("legacy-agent".to_string());
        session.messages = vec![
            Message::new(
                MessageRole::System,
                MessageContent::Text("old stored prompt".to_string()),
            ),
            Message::new(
                MessageRole::User,
                MessageContent::Text("earlier user".to_string()),
            ),
        ];

        let mut agent = harnx_core::agent_config::AgentConfig::from_prompt("fresh rendered prompt");
        agent.set_name("legacy-agent");
        let input = Input::new(
            "follow up".to_string(),
            ("follow up".to_string(), vec![]),
            agent,
        );
        let messages = super::build_messages(&session, &input).unwrap();

        let system_messages: Vec<_> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == MessageRole::System)
            .collect();
        assert_eq!(system_messages.len(), 1, "messages: {messages:#?}");
        assert_eq!(system_messages[0].0, 0, "messages: {messages:#?}");
        assert_eq!(messages[0].content.to_text(), "fresh rendered prompt");
    }

    #[test]
    fn build_messages_tool_continuation_still_injects_system_prompt() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;

        let mut session = test_session();
        session.messages = vec![
            Message::new(MessageRole::User, MessageContent::Text("q".to_string())),
            Message::new(
                MessageRole::Tool,
                MessageContent::ToolCalls(crate::client::MessageContentToolCalls::new(
                    vec![ToolResult::new(
                        ToolCall::new(
                            "fs_read".to_string(),
                            json!({"path": "README.md"}),
                            Some("call_1".to_string()),
                            None,
                        ),
                        json!({"content": "ok"}),
                    )],
                    "tool round".to_string(),
                    None,
                )),
            ),
        ];

        let mut input = Input::new(
            "follow up".to_string(),
            ("follow up".to_string(), vec![]),
            harnx_core::agent_config::AgentConfig::from_prompt("fresh tool continuation prompt"),
        );
        input.tool_calls = Some(crate::client::MessageContentToolCalls::new(
            vec![],
            "continuation".to_string(),
            None,
        ));

        let messages = super::build_messages(&session, &input).unwrap();
        assert_eq!(
            messages[0].role,
            MessageRole::System,
            "messages: {messages:#?}"
        );
        assert_eq!(
            messages[0].content.to_text(),
            "fresh tool continuation prompt"
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.role == MessageRole::User)
                .count(),
            1,
            "messages: {messages:#?}"
        );
        assert_eq!(messages[1].content.to_text(), "q");
    }
}
