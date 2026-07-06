use super::input::*;
use super::session_externalize::{
    externalize_content, externalize_persisted_messages, externalize_tool_result_content,
    record_externalized,
};
use super::*;
use crate::nats_client_session::new_client_message_id;

pub use harnx_core::session::{Session, SessionLogEntry};

use std::any::Any;
use std::sync::{Arc, Mutex};

pub trait SessionAppendSink: Send + Sync + Any {
    fn append(&self, entry: &SessionLogEntry) -> Result<u64>;
}

#[derive(Debug)]
pub(crate) struct FileSessionLogSink {
    log: Mutex<FileSessionLog>,
}

impl FileSessionLogSink {
    pub(crate) fn new(path: &Path, session_name: &str, header: SessionLogEntry) -> Self {
        Self {
            log: Mutex::new(FileSessionLog::new_with_header(path, session_name, header)),
        }
    }
}

impl SessionAppendSink for FileSessionLogSink {
    fn append(&self, entry: &SessionLogEntry) -> Result<u64> {
        let mut log = self
            .log
            .lock()
            .map_err(|_| anyhow::anyhow!("file session log sink mutex poisoned"))?;
        log.append_event(entry)
    }
}

use crate::client::{CompletionTokenUsage, Message, MessageContent, MessageRole};
use harnx_core::{
    event::{AgentEvent, SessionEvent},
    session_log::SessionLog,
    sink::emit_agent_event,
};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use std::fs::{read_to_string, write, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use crate::utils::{
    session_name::{decode_timestamp_session_id, generate_session_id, git_branch, git_remote},
    terminal_session_id,
};

pub fn new(config: &Config, name: &str, working_dir: Option<&std::path::Path>) -> Result<Session> {
    let agent = config.extract_agent();
    let session_id = if uuid::Uuid::parse_str(name)
        .ok()
        .is_some_and(|uuid| uuid.get_version_num() == 7)
        || decode_timestamp_session_id(name).is_some()
    {
        name.to_string()
    } else {
        generate_session_id(|candidate| config.session_file(candidate).exists())
    };
    let mut session = Session {
        id: name.to_string(),
        save_session: config.save_session,
        session_id: Some(session_id),
        working_dir: working_dir
            .map(std::path::Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .map(|path| path.to_string_lossy().into_owned()),
        git_branch: {
            let b = git_branch();
            if b.is_empty() {
                None
            } else {
                Some(b)
            }
        },
        git_remote: git_remote(),
        terminal_session_id: terminal_session_id(),
        ..Default::default()
    };
    session.set_agent(&agent)?;
    session.dirty = false;
    Ok(session)
}

pub fn load(config: &Config, name: &str, path: &Path) -> Result<Session> {
    let content = read_to_string(path)
        .with_context(|| format!("Failed to load session {} at {}", name, path.display()))?;

    // Detect format: new log format has "type: header" as the first
    // meaningful line. Old format files are silently treated as empty
    // sessions (no crash, but content is not loaded).
    let session = if Session::is_log_format(&content) {
        load_from_log(config, name, path, &content)?
    } else {
        // Old format: create a fresh session so we don't crash.
        let mut session = new(config, name, None)?;
        apply_name_and_path(&mut session, name, path, config)?;
        session
    };

    Ok(session)
}

fn load_from_log(config: &Config, name: &str, path: &Path, content: &str) -> Result<Session> {
    let log = FileSessionLog::new(path, name);
    let raw_entries = log.load_events()?;
    debug_assert_eq!(
        raw_entries.len(),
        collect_raw_log_entries(content, name)?.len(),
        "FileSessionLog::load_events changed entry count"
    );
    let replay_entries: Vec<_> = raw_entries
        .iter()
        .map(|(seq, entry)| (*seq as usize, entry.clone()))
        .collect();
    let mut session = replay_log_entries_for_external(&replay_entries, name)?;
    session.log_entry_count = raw_entries.len();

    session.model =
        crate::client::retrieve_model(&config.clients, &session.model_id, ModelType::Chat)?;
    apply_name_and_path(&mut session, name, path, config)?;
    session.update_tokens();
    Ok(session)
}

struct PendingToolCalls {
    seq: usize,
    text: String,
    thought: Option<String>,
    calls: Vec<crate::tool::ToolCall>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

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

pub(crate) fn replay_log_entries_for_external(
    raw_entries: &[(usize, SessionLogEntry)],
    name: &str,
) -> Result<Session> {
    let effective_entries = build_effective_log_entries(raw_entries, name);
    let mut session = Session::default();

    // Pending ToolCalls entry awaiting a matching ToolResults entry.
    // On any other entry (or EOF) while pending, we repair by
    // synthesizing lost-response errors for each pending call — this
    // only matters for the tail of the log (crash mid tool round);
    // mid-log corruption would be an invariant violation.
    let mut pending: Option<PendingToolCalls> = None;

    for (seq, entry) in effective_entries {
        match entry {
            SessionLogEntry::Header {
                model_id,
                temperature,
                top_p,
                use_tools,
                save_session,
                compress_threshold,
                agent_name,
                session_id,
                working_dir,
                git_branch,
                git_remote,
                terminal_session_id,
                agent_variables,
                agent_instructions,
                model_fallbacks,
                compaction_agent,
            } => {
                session.model_id = model_id;
                session.temperature = temperature;
                session.top_p = top_p;
                session.use_tools = use_tools;
                session.save_session = save_session;
                session.compress_threshold = compress_threshold;
                session.agent_name = agent_name;
                session.session_id = session_id;
                session.working_dir = working_dir;
                session.git_branch = git_branch;
                session.git_remote = git_remote;
                session.terminal_session_id = terminal_session_id;
                session.agent_variables = agent_variables;
                session.agent_instructions = agent_instructions;
                session.model_fallbacks = model_fallbacks;
                session.compaction_agent = compaction_agent;
            }
            SessionLogEntry::Message {
                id,
                role,
                content,
                timestamp,
                ..
            } => {
                if let Some(pending) = pending.take() {
                    session
                        .messages
                        .push(repair_orphan_tool_calls(pending, name)?);
                }
                if role == MessageRole::Tool {
                    anyhow::bail!(
                        "Invalid log entry in session {name}: Tool-role Message entries are                          no longer supported; use tool_calls/tool_results entries"

                    );
                }
                let mut message = Message::new(role, content).with_log_seq(seq);
                if let Some(id) = id {
                    message = message.with_id(id);
                }
                if let Some(timestamp) = timestamp {
                    message = message.with_log_timestamp(timestamp);
                }
                session.messages.push(message);
            }
            SessionLogEntry::ToolCalls {
                text,
                thought,
                calls,
                timestamp,
                ..
            } => {
                if let Some(pending) = pending.take() {
                    session
                        .messages
                        .push(repair_orphan_tool_calls(pending, name)?);
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
                    anyhow::bail!(
                        "Invalid log entry in session {name}: tool_results without a                          preceding tool_calls entry"
                    );
                };
                let mut message =
                    assemble_tool_message(text, thought, calls, results).with_log_seq(seq);
                if let Some(timestamp) = timestamp {
                    message = message.with_log_timestamp(timestamp);
                }
                session.messages.push(message);
            }
            SessionLogEntry::DataUrls { urls } => {
                session.data_urls.extend(urls);
            }
            SessionLogEntry::Compress { prompt } => {
                if let Some(pending) = pending.take() {
                    session
                        .messages
                        .push(repair_orphan_tool_calls(pending, name)?);
                }
                session.compressed_messages.append(&mut session.messages);
                session.messages.push(Message::new(
                    MessageRole::System,
                    MessageContent::Text(prompt),
                ));
            }
            SessionLogEntry::Clear => {
                pending = None;
                session.messages.clear();
                session.compressed_messages.clear();
                session.data_urls.clear();
            }
            SessionLogEntry::Cancel { .. } => {}
            SessionLogEntry::EditEntries { .. }
            | SessionLogEntry::Rewind { .. }
            | SessionLogEntry::Unknown => {}
        }
    }

    if let Some(pending) = pending.take() {
        session
            .messages
            .push(repair_orphan_tool_calls(pending, name)?);
    }

    Ok(session)
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
        output: serde_json::json!({
            "error": "tool response lost (session was interrupted before results were persisted)"
        }),
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
    Ok(message)
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
                    content: out.content,
                    switch_agent: out.switch_agent,
                },
                None => ToolResult::new(
                    call,
                    serde_json::json!({
                        "error": "tool response lost (session was interrupted before results were persisted)"
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

#[derive(Debug)]
pub(crate) struct FileSessionLog {
    path: std::path::PathBuf,
    session_name: String,
    next_seq: Option<u64>,
    initial_header: Option<SessionLogEntry>,
}

impl FileSessionLog {
    fn new(path: &Path, session_name: &str) -> Self {
        Self {
            path: path.to_path_buf(),
            session_name: session_name.to_string(),
            next_seq: None,
            initial_header: None,
        }
    }

    fn new_with_header(path: &Path, session_name: &str, header: SessionLogEntry) -> Self {
        Self {
            path: path.to_path_buf(),
            session_name: session_name.to_string(),
            next_seq: None,
            initial_header: Some(header),
        }
    }
}

impl SessionLog for FileSessionLog {
    fn append_event(&mut self, entry: &SessionLogEntry) -> Result<u64> {
        let assigned_seq = match self.next_seq {
            Some(seq) => seq,
            None => match self.load_events() {
                Ok(entries) => entries.len() as u64,
                Err(_)
                    if !self.path.exists()
                        || self
                            .path
                            .metadata()
                            .is_ok_and(|metadata| metadata.len() == 0) =>
                {
                    let Some(header) = &self.initial_header else {
                        return self.load_events().map(|entries| entries.len() as u64);
                    };
                    ensure_parent_exists(&self.path)?;
                    let content = serde_yaml::to_string(header).with_context(|| {
                        format!(
                            "Failed to serialize session header in '{}'",
                            self.session_name
                        )
                    })?;
                    write(&self.path, content).with_context(|| {
                        format!(
                            "Failed to initialize session {} at {}",
                            self.session_name,
                            self.path.display()
                        )
                    })?;
                    1
                }
                Err(err) => return Err(err),
            },
        };
        let yaml = serde_yaml::to_string(entry)
            .with_context(|| format!("Failed to serialize log entry in '{}'", self.session_name))?;
        let mut data = String::from("---\n");
        data.push_str(&yaml);
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        file.write_all(data.as_bytes())?;
        self.next_seq = Some(assigned_seq + 1);
        Ok(assigned_seq)
    }

    fn load_events(&self) -> Result<Vec<(u64, SessionLogEntry)>> {
        let content = read_to_string(&self.path).with_context(|| {
            format!(
                "Failed to load session {} at {}",
                self.session_name,
                self.path.display()
            )
        })?;
        collect_raw_log_entries(&content, &self.session_name).map(|entries| {
            entries
                .into_iter()
                .map(|(seq, entry)| (seq as u64, entry))
                .collect()
        })
    }

    fn replay_from(&self, seq: u64) -> Result<Vec<SessionLogEntry>> {
        Ok(self
            .load_events()?
            .into_iter()
            .filter(|(entry_seq, _)| *entry_seq >= seq)
            .map(|(_, entry)| entry)
            .collect())
    }
}

fn apply_name_and_path(
    session: &mut Session,
    name: &str,
    path: &Path,
    config: &Config,
) -> Result<()> {
    session.id = name.to_string();
    session.path = Some(path.display().to_string());

    session.agent_prompt = session.agent_instructions.clone();
    if let Some(agent_name) = &session.agent_name {
        if let Ok(agent) = config.retrieve_agent(agent_name) {
            // Only re-render the prompt when the session does not already have
            // resolved agent data from the log.  If agent_variables is
            // non-empty the session was restored from disk with its own
            // variable values; re-rendering with the current agent definition
            // would overwrite those resolved values.  Similarly, if
            // agent_prompt differs from agent_instructions the session log
            // already stored a rendered prompt — preserve it.
            let prompt_is_unresolved = session.agent_variables().is_empty()
                && session.agent_prompt == session.agent_instructions;
            if prompt_is_unresolved {
                session.agent_prompt = agent.interpolated_instructions()?;
            }
            if session.use_tools.is_none() {
                session.use_tools = agent.use_tools();
            }
            if session.model_fallbacks.is_empty() {
                session.model_fallbacks = agent.model_fallbacks().to_vec();
            }
            if session.compaction_agent.is_none() {
                session.compaction_agent = agent.compaction_agent().map(str::to_string);
            }
        }
    }
    Ok(())
}

/// Initialize the session log file with a header entry.
/// Called lazily on the first append_event when a path hasn't been
/// established yet.  Best-effort: filesystem errors are silently
/// ignored so the session can still be used in-memory.
pub fn ensure_log_file(session: &mut Session) {
    if session.save_session() == Some(false) {
        return;
    }
    if session.path.is_some() {
        return;
    }
    let Some(sessions_dir) = session.sessions_dir.clone() else {
        return;
    };

    let session_path = sessions_dir.join(format!("{}.yaml", session.id));
    if ensure_parent_exists(&session_path).is_err() {
        return;
    }

    let header = session.build_header_entry();
    let Ok(content) = serde_yaml::to_string(&header) else {
        return;
    };
    if write(&session_path, &content).is_ok() {
        session.path = Some(session_path.display().to_string());
        session.log_entry_count = 1;
    }
}

/// Append a log entry to the session file.
/// Lazily initializes the log file on the first call.
/// Returns true if the entry was successfully written.
pub fn append_event(session: &mut Session, entry: &SessionLogEntry) -> bool {
    if let Some(runtime) = session.runtime.as_ref() {
        if let Some(append_sink) = runtime.downcast_ref::<Arc<dyn SessionAppendSink>>() {
            return match append_sink.append(entry) {
                Ok(seq) => {
                    session.log_entry_count = seq as usize + 1;
                    true
                }
                Err(_) => false,
            };
        }
    }

    ensure_log_file(session);
    let Some(path_str) = &session.path else {
        return false;
    };
    let mut log = FileSessionLog::new(Path::new(path_str), &session.id);
    match log.append_event(entry) {
        Ok(seq) => {
            session.log_entry_count = seq as usize + 1;
            true
        }
        Err(_) => false,
    }
}

pub fn render(session: &Session) -> Result<String> {
    let mut items = vec![];

    if let Some(path) = &session.path {
        items.push(("path", path.to_string()));
    }

    items.push((
        "model",
        format!(
            "{} (vision: {})",
            session.model().id(),
            session.model().supports_vision()
        ),
    ));

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

    if let Some(save_session) = session.save_session() {
        items.push(("save_session", save_session.to_string()));
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

pub fn exit(session: &mut Session, session_dir: &Path, is_tui: bool) -> Result<()> {
    if session.save_session() == Some(false) && !session.save_session_this_time {
        return Ok(());
    }
    if !session.dirty {
        // Nothing new to persist, but print the path if the log file exists.
        if is_tui {
            if let Some(path) = &session.path {
                crate::utils::emit_info(format!("✓ Session saved at '{path}'."));
            }
        }
        return Ok(());
    }
    // Session has unsaved changes that were not yet appended (e.g. legacy
    // callers or sessions that didn't go through init_log). Do a full save.
    let session_name = session.id.clone();
    let session_path = session_dir.join(format!("{}.yaml", session.id));
    save(session, &session_name, &session_path, is_tui)?;
    Ok(())
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

fn append_message_entries(content: &mut String, msg: &Message, session_id: &str) -> Result<()> {
    if msg.role == MessageRole::Tool {
        if let MessageContent::ToolCalls(tc) = &msg.content {
            let calls: Vec<crate::tool::ToolCall> =
                tc.tool_results.iter().map(|r| r.call.clone()).collect();
            let tool_calls_entry = SessionLogEntry::ToolCalls {
                text: tc.text.clone(),
                thought: tc.thought.clone(),
                calls,
                timestamp: None,
                fence_token: None,
            };
            content.push_str("---\n");
            content
                .push_str(&serde_yaml::to_string(&tool_calls_entry).with_context(|| {
                    format!("Failed to serialize tool_calls in '{session_id}'")
                })?);

            let results: Vec<harnx_core::session::ToolOutput> = tc
                .tool_results
                .iter()
                .map(|r| harnx_core::session::ToolOutput {
                    id: r.call.id.clone(),
                    name: r.call.name.clone(),
                    output: r.output.clone(),
                    content: r.content.clone(),
                    switch_agent: r.switch_agent.clone(),
                })
                .collect();
            let tool_results_entry = SessionLogEntry::ToolResults {
                results,
                timestamp: None,
            };
            content.push_str("---\n");
            content.push_str(
                &serde_yaml::to_string(&tool_results_entry).with_context(|| {
                    format!("Failed to serialize tool_results in '{session_id}'")
                })?,
            );
            return Ok(());
        }
    }

    let entry = SessionLogEntry::Message {
        id: msg.id.clone().or_else(|| Some(persisted_message_id())),
        role: msg.role,
        content: msg.content.clone(),
        timestamp: None,
        fence_token: None,
    };
    content.push_str("---\n");
    content.push_str(
        &serde_yaml::to_string(&entry)
            .with_context(|| format!("Failed to serialize message in '{session_id}'"))?,
    );
    Ok(())
}

/// Full save: rewrites the entire session file in log format.
/// Used as a fallback when events were not incrementally appended.
pub fn save(
    session: &mut Session,
    session_name: &str,
    session_path: &Path,
    is_tui: bool,
) -> Result<()> {
    ensure_parent_exists(session_path)?;

    session.path = Some(session_path.display().to_string());

    // Externalize any still-inline image data URIs across all messages (e.g.
    // first-turn images persisted before incremental externalization, or
    // legacy sessions loaded with inline base64) so the rewritten transcript
    // never carries inline base64. Idempotent: existing cid refs are skipped.
    externalize_persisted_messages(session, session_path);

    // Write in the new log format.
    let mut content = serde_yaml::to_string(&session.build_header_entry())
        .with_context(|| format!("Failed to serialize session header for '{}'", session.id))?;
    for msg in &session.compressed_messages {
        append_message_entries(&mut content, msg, &session.id)?;
    }
    if !session.compressed_messages.is_empty() {
        // Write a compress entry to mark the boundary.
        // Only write it and skip the first message if the first message
        // is actually a system message from compression.
        let wrote_compress = if let Some(system_msg) = session.messages.first() {
            if system_msg.role == MessageRole::System {
                let compress_entry = SessionLogEntry::Compress {
                    prompt: system_msg.content.to_text(),
                };
                content.push_str("---\n");
                content.push_str(&serde_yaml::to_string(&compress_entry).with_context(|| {
                    format!("Failed to serialize compress entry in '{}'", session.id)
                })?);
                true
            } else {
                false
            }
        } else {
            false
        };
        // Write remaining messages (skip the system message from compress only if we wrote a compress entry).
        let start_idx = if wrote_compress { 1 } else { 0 };
        for msg in session.messages.iter().skip(start_idx) {
            append_message_entries(&mut content, msg, &session.id)?;
        }
    } else {
        for msg in &session.messages {
            append_message_entries(&mut content, msg, &session.id)?;
        }
    }
    if !session.data_urls.is_empty() {
        let entry = SessionLogEntry::DataUrls {
            urls: session.data_urls.clone(),
        };
        content.push_str("---\n");
        content.push_str(
            &serde_yaml::to_string(&entry)
                .with_context(|| format!("Failed to serialize data_urls in '{}'", session.id))?,
        );
    }

    write(session_path, &content).with_context(|| {
        format!(
            "Failed to write session '{}' to '{}'",
            session.id,
            session_path.display()
        )
    })?;

    if is_tui {
        crate::utils::emit_info(format!(
            "✓ Saved the session to '{}'.",
            session_path.display()
        ));
    }

    if session.id() != session_name {
        session.id = session_name.to_string()
    }

    session.log_entry_count = serde_yaml::Deserializer::from_str(&content).count();
    session.dirty = false;

    Ok(())
}

pub fn to_agent(session: &Session) -> Agent {
    Agent::new(
        session
            .to_agent_config()
            .expect("session agent config should always be valid"),
    )
}

pub fn compress(session: &mut Session, mut prompt: String) {
    if let Some(system_prompt) = session.messages.first().and_then(|v| {
        if MessageRole::System == v.role {
            let content = v.content.to_text();
            if !content.is_empty() {
                return Some(content);
            }
        }
        None
    }) {
        prompt = format!("{system_prompt}\n\n{prompt}",);
    }
    session.compressed_messages.append(&mut session.messages);
    session.messages.push(Message::new(
        MessageRole::System,
        MessageContent::Text(prompt.clone()),
    ));
    session.update_tokens();
    if !append_event(session, &SessionLogEntry::Compress { prompt }) {
        session.dirty = true;
    }
}

/// Compact only the prefix `messages[..keep_from]`, keeping `messages[keep_from..]`
/// verbatim. The prefix moves to `compressed_messages`; the new message list is
/// `[summary system message, ...kept suffix]`. The original leading system
/// prompt (if any) is folded into the summary, matching `compress`.
pub fn compress_keeping_recent(session: &mut Session, mut prompt: String, keep_from: usize) {
    let keep_from = keep_from.min(session.messages.len());
    if let Some(system_prompt) = session.messages.first().and_then(|v| {
        if MessageRole::System == v.role {
            let content = v.content.to_text();
            if !content.is_empty() {
                return Some(content);
            }
        }
        None
    }) {
        prompt = format!("{system_prompt}\n\n{prompt}",);
    }
    // Split off the recent suffix to keep verbatim; the remainder is the prefix.
    let suffix: Vec<Message> = session.messages.split_off(keep_from);
    session.compressed_messages.append(&mut session.messages);
    // Hard-cut log layout: the Compress event archives the prefix and carries
    // the summary; the preserved suffix is then re-logged as fresh entries so
    // replay reproduces `[summary, ...suffix]` without any stored index.
    session.messages.push(Message::new(
        MessageRole::System,
        MessageContent::Text(prompt.clone()),
    ));
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
/// event so the on-disk layout is self-describing (no stored index).
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
    }
    session.update_tokens();
    Ok(())
}

#[cfg(test)]
mod working_dir_tests {
    use super::*;
    use crate::tool::ToolCall;
    use harnx_core::input::Input;
    use harnx_core::message::{Message, MessageContent, MessageRole};
    use harnx_core::session::SessionLogEntry;
    use std::fs;
    use tempfile::TempDir;

    fn make_message(content: &str) -> Message {
        Message {
            role: MessageRole::User,
            content: MessageContent::Text(content.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn new_uses_explicit_working_dir_when_provided() {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("data").join("sessions")).unwrap();
        fs::create_dir_all(root.path().join("state")).unwrap();
        let config = Config {
            sessions_dir_override: Some(root.path().join("sessions")),
            ..Default::default()
        };
        fs::create_dir_all(config.sessions_dir()).unwrap();
        let working_dir = root.path().join("repo-a");
        fs::create_dir_all(&working_dir).unwrap();

        let session = new(&config, "session-a", Some(&working_dir)).unwrap();
        assert_eq!(
            session.working_dir.as_deref(),
            Some(working_dir.to_str().unwrap())
        );
    }

    #[test]
    fn session_log_header_persists_distinct_working_dirs_per_session() {
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join("data").join("sessions")).unwrap();
        fs::create_dir_all(root.path().join("state")).unwrap();
        let config = Config {
            sessions_dir_override: Some(root.path().join("sessions")),
            ..Default::default()
        };
        fs::create_dir_all(config.sessions_dir()).unwrap();
        let working_dir_a = root.path().join("repo-a");
        let working_dir_b = root.path().join("repo-b");
        fs::create_dir_all(&working_dir_a).unwrap();
        fs::create_dir_all(&working_dir_b).unwrap();

        let mut session_a = new(&config, "session-a", Some(&working_dir_a)).unwrap();
        session_a.messages.push(make_message("hello from a"));
        add_tool_calls(
            &mut session_a,
            &Input::new(
                "".to_string(),
                ("".to_string(), vec![]),
                config.extract_agent().into_config(),
            ),
            "out",
            None,
            &[ToolCall::new(
                "noop".to_string(),
                serde_json::json!({}),
                None,
                None,
            )],
        )
        .unwrap();
        save(
            &mut session_a,
            "session-a",
            &config.session_file("session-a"),
            false,
        )
        .unwrap();
        let log_a = fs::read_to_string(config.session_file("session-a")).unwrap();

        let mut session_b = new(&config, "session-b", Some(&working_dir_b)).unwrap();
        session_b.messages.push(make_message("hello from b"));
        save(
            &mut session_b,
            "session-b",
            &config.session_file("session-b"),
            false,
        )
        .unwrap();
        let log_b = fs::read_to_string(config.session_file("session-b")).unwrap();

        let header_a =
            serde_yaml::from_str::<SessionLogEntry>(log_a.split("\n---\n").next().unwrap())
                .unwrap();
        let header_b =
            serde_yaml::from_str::<SessionLogEntry>(log_b.split("\n---\n").next().unwrap())
                .unwrap();

        match header_a {
            SessionLogEntry::Header { working_dir, .. } => {
                assert_eq!(
                    working_dir.as_deref(),
                    Some(working_dir_a.to_str().unwrap())
                );
            }
            other => panic!("expected header entry, got {other:?}"),
        }
        match header_b {
            SessionLogEntry::Header { working_dir, .. } => {
                assert_eq!(
                    working_dir.as_deref(),
                    Some(working_dir_b.to_str().unwrap())
                );
            }
            other => panic!("expected header entry, got {other:?}"),
        }
        assert_ne!(working_dir_a, working_dir_b);
    }
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
                    "error": "tool response pending (results not yet persisted)"
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
    let attachments_dir = session
        .path
        .as_deref()
        .map(|p| crate::config::attachments::attachments_dir_for(std::path::Path::new(p)));
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
    // before persisting, freeing the in-memory base64. Files are written now;
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
    if !appended {
        session.dirty = true;
    }
    if !record_externalized(session, cid_urls) {
        session.dirty = true;
    }
    session.update_tokens();
    Ok(())
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

pub fn clear_messages(session: &mut Session) {
    session.messages.clear();
    session.compressed_messages.clear();
    session.data_urls.clear();
    session.completion_usage = CompletionTokenUsage::default();
    session.update_tokens();
    if !append_event(session, &SessionLogEntry::Clear) {
        session.dirty = true;
    }
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
    // No longer call expand_message_attachments here - it's now conditional
    // based on client capability, called from prepare_completion_data
    Ok(messages)
}

/// Expand the `cid:` image references in a single message's content — both
/// plain content arrays and tool-result content — to inline `data:` URIs.
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
            let mut res = Ok(());
            for tool_result in tool_calls.tool_results.iter_mut() {
                if let Err(err) =
                    crate::config::attachments::expand_parts(encoder, dir, &mut tool_result.content)
                {
                    res = Err(err);
                }
            }
            res
        }
        MessageContent::Text(_) => Ok(()),
    };
    if let Err(err) = result {
        log::warn!("attachment expansion failed: {err}");
    }
}

/// Replace `cid:` image references in outgoing messages with inline `data:`
/// URIs (base64 backend). Walks both plain content arrays and tool-result
/// content. No-op when the session has no attachments dir / no cid refs.
///
/// NOTE: This function is NO LONGER called from `build_messages`. The base64
/// pre-pass is now conditional on the client capability:
/// - For clients with `expands_attachments_internally() == true` (Gemini native),
///   `prepare_completion_data` skips this entirely, leaving raw `cid:` refs.
/// - For all other providers, this is called from `prepare_completion_data` to
///   expand `cid:` refs to base64 data: URLs before the client sees them.
pub(crate) fn expand_message_attachments(session: &Session, messages: &mut [Message]) {
    // A `cid:` ref only ever enters a message via the externalize-on-save path,
    // which requires a session path — so no path means no refs to expand.
    let Some(path) = session.path.as_ref() else {
        return;
    };

    let dir = crate::config::attachments::attachments_dir_for(std::path::Path::new(path));
    let encoder = crate::config::attachments::Base64Encoder;
    for message in messages.iter_mut() {
        expand_message(&encoder, &dir, &mut message.content);
    }
}

fn build_messages_inner(session: &Session, input: &Input) -> Result<Vec<Message>> {
    let mut messages = session.messages.clone();
    if input.continue_output().is_some() {
        return Ok(messages);
    } else if input.regenerate() {
        while let Some(last) = messages.last() {
            if !last.role.is_user() {
                messages.pop();
            } else {
                break;
            }
        }
        return Ok(messages);
    }
    let mut need_add_msg = true;
    let len = messages.len();
    if len == 0 {
        messages = input.agent().build_messages(input)?;
        need_add_msg = false;
    } else if len == 1 && session.compressed_messages.len() >= 2 {
        if let Some(index) = session
            .compressed_messages
            .iter()
            .rposition(|v| v.role == MessageRole::User)
        {
            messages.extend(session.compressed_messages[index..].to_vec());
        }
    }
    // Continuation: suppress the duplicate user message only when the
    // input is genuinely mid-tool-round — see `is_tool_continuation`.
    if need_add_msg && is_tool_continuation(input, &messages) {
        need_add_msg = false;
    }
    if need_add_msg {
        // When the agent was swapped after construction (e.g. compaction),
        // inject_system_prompt is true and we must prepend the agent's
        // system prompt — session messages won't already contain it.
        // On normal session turns the system prompt was stored on turn 1
        // by save_message(), so inject_system_prompt stays false.
        if input.inject_system_prompt() {
            let system_text = input.agent().system_text()?;
            if !system_text.is_empty() {
                messages.insert(
                    0,
                    Message::new(MessageRole::System, MessageContent::Text(system_text)),
                );
            }
        }
        messages.push(Message::new(MessageRole::User, input.message_content()));
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> Session {
        new(&Config::default(), "test", None).unwrap()
    }

    fn message_view(messages: &[Message]) -> Vec<(MessageRole, String)> {
        messages
            .iter()
            .map(|m| (m.role, m.content.to_text()))
            .collect()
    }

    #[test]
    fn load_from_log_enumerates_document_sequence_numbers() {
        let content = r#"---
type: header
model: openai:gpt-4o
---
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

        assert_eq!(seqs.len(), 5);
        assert!(matches!(seqs[0], (0, SessionLogEntry::Header { .. })));
        assert!(matches!(
            seqs[1],
            (
                1,
                SessionLogEntry::Message {
                    timestamp: None,
                    ..
                }
            )
        ));
        assert!(matches!(
            seqs[2],
            (2, SessionLogEntry::Rewind { after_seq: 1 })
        ));
        assert!(matches!(
            seqs[3],
            (3, SessionLogEntry::EditEntries { from: 1, to: 1, .. })
        ));
        assert!(matches!(
            seqs[4],
            (
                4,
                SessionLogEntry::Message {
                    timestamp: None,
                    ..
                }
            )
        ));

        let session = super::load_from_log_for_test(content);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content.to_text(), "second");
        assert_eq!(session.log_entry_count, 5);
        assert_eq!(session.next_seq(), 5);
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

        // Live messages: summary system message followed by the kept suffix.
        assert_eq!(
            message_view(&session.messages),
            vec![
                (
                    MessageRole::System,
                    "summary of earlier conversation".to_string()
                ),
                (MessageRole::User, "recent question".to_string()),
                (MessageRole::Assistant, "recent answer".to_string()),
            ]
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

        assert_eq!(
            message_view(&session.messages),
            vec![(MessageRole::System, "summary".to_string())]
        );
        assert_eq!(
            message_view(&session.compressed_messages),
            vec![
                (MessageRole::User, "first".to_string()),
                (MessageRole::Assistant, "second".to_string()),
            ]
        );
    }

    #[test]
    fn compress_keeping_recent_reload_matches_in_memory() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        session.messages = vec![
            Message::new(
                MessageRole::System,
                MessageContent::Text("base prompt".to_string()),
            ),
            Message::new(MessageRole::User, MessageContent::Text("q1".to_string())),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("a1".to_string()),
            ),
            Message::new(MessageRole::User, MessageContent::Text("q2".to_string())),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("a2".to_string()),
            ),
        ];

        // Persist the message entries to a real log file so the compress entry
        // is appended incrementally on top of them.
        super::save(&mut session, "test", &tmp.path().join("test.yaml"), false).unwrap();

        // Compact the prefix, keeping the last two messages verbatim.
        super::compress_keeping_recent(&mut session, "rolling summary".to_string(), 3);

        let in_memory = message_view(&session.messages);

        let persisted = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        let reloaded = super::load_from_log_for_test(&persisted);

        assert_eq!(
            message_view(&reloaded.messages),
            in_memory,
            "reloaded messages must equal the in-memory result of compress_keeping_recent"
        );
        // Sanity: the recent suffix survived rather than being dropped.
        assert!(reloaded
            .messages
            .iter()
            .any(|m| m.content.to_text() == "q2"));
        assert!(reloaded
            .messages
            .iter()
            .any(|m| m.content.to_text() == "a2"));
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

    /// Test that timestamps are persisted to log entries and survive reload.
    #[test]
    fn timestamp_persists_across_session_reload() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(&global_config, "hello", Some(agent.clone()));

        // Add a user message
        super::begin_turn(&mut session, &input, "response").unwrap();

        // Verify the log file contains the timestamp field
        let path = session.path.clone().unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("timestamp:"),
            "log should contain timestamp field"
        );

        // Parse the log entries directly
        let entries: Vec<(usize, SessionLogEntry)> = serde_yaml::Deserializer::from_str(&content)
            .enumerate()
            .map(|(seq, doc)| {
                let entry = SessionLogEntry::deserialize(doc).expect("valid entry");
                (seq, entry)
            })
            .collect();

        // Find a Message entry and verify it has a timestamp
        for (_, entry) in &entries {
            if let SessionLogEntry::Message { timestamp, .. } = entry {
                assert!(timestamp.is_some(), "message entry should have a timestamp");
                return;
            }
        }
        panic!("no Message entry found in session log");
    }

    /// The tool round splits into two independent log entries: a
    /// `tool_calls` event written immediately after the LLM returns,
    /// and a matching `tool_results` event after execution. In memory
    /// they collapse into a single `Message(Tool, ToolCalls)` carrying
    /// the outputs.
    #[test]
    fn add_tool_calls_and_results_saves_two_entries() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(&global_config, "hello", Some(agent.clone()));

        let call = ToolCall {
            name: "test_tool".to_string(),
            arguments: json!({"arg": "val"}),
            id: Some("call_1".to_string()),
            thought_signature: None,
        };

        super::add_tool_calls(
            &mut session,
            &input,
            "I'll call a tool",
            None,
            std::slice::from_ref(&call),
        )
        .unwrap();
        // Before results arrive, the in-memory last message is a
        // pending Tool message with placeholder error outputs.
        assert_eq!(session.messages.last().unwrap().role, MessageRole::Tool);

        let results = vec![ToolResult::new(call, json!({"result": "ok"}))];
        super::add_tool_results(&mut session, &results).unwrap();

        // Check the in-memory outputs got filled in.
        let last = session.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::Tool);
        let MessageContent::ToolCalls(tc) = &last.content else {
            panic!("expected ToolCalls content");
        };
        assert_eq!(tc.tool_results.len(), 1);
        assert_eq!(tc.tool_results[0].output, json!({"result": "ok"}));

        // On disk: separate ToolCalls and ToolResults events.
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        assert!(
            content.contains("type: tool_calls"),
            "file should contain a tool_calls entry"
        );
        assert!(
            content.contains("type: tool_results"),
            "file should contain a tool_results entry"
        );
        assert!(
            content.contains("test_tool"),
            "file should contain the tool name"
        );

        // Now a second round with a plain text reply — continuation
        // detection should skip the duplicate user message.
        // The agent loop always calls merge_tool_results before the next
        // LLM call (setting tool_calls on the input), so the continuation
        // input must carry those tool results to be recognised as a
        // mid-round continuation rather than a fresh user prompt.
        let input2 = input.merge_tool_results("I'll call a tool".to_string(), None, results);
        super::add_assistant_text(&mut session, &input2, "final answer", None).unwrap();

        let user_count = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count();
        assert_eq!(
            user_count, 1,
            "continuation detection should prevent duplicate user messages"
        );
        assert_eq!(
            session.messages.last().unwrap().content.to_text(),
            "final answer"
        );
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

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

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

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

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

    /// Regression test: when the LLM emits multiple tool calls with
    /// the same id (rare but observed, e.g. around agent handoffs),
    /// `eval_tool_calls` dedupes before execution. `add_tool_calls`
    /// must dedup identically so the pending slots / log entries match
    /// the eventual results — otherwise the unmatched pending slot
    /// persists as a "tool response pending" placeholder and the LLM
    /// sees two results with the same tool_use_id on the next turn.
    #[test]
    fn add_tool_calls_dedupes_duplicate_ids() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(&global_config, "run bash", Some(agent));

        // Two calls share an id (LLM bug); one has a unique id.
        let dup_1 = ToolCall {
            name: "Bash".to_string(),
            arguments: json!({"command": "pwd"}),
            id: Some("toolu_dup".to_string()),
            thought_signature: None,
        };
        let dup_2 = ToolCall {
            name: "Bash".to_string(),
            arguments: json!({"command": "ls"}),
            id: Some("toolu_dup".to_string()),
            thought_signature: None,
        };
        let unique = ToolCall {
            name: "Bash".to_string(),
            arguments: json!({"command": "echo hi"}),
            id: Some("toolu_unique".to_string()),
            thought_signature: None,
        };

        super::add_tool_calls(
            &mut session,
            &input,
            "calling tools",
            None,
            &[dup_1, dup_2.clone(), unique.clone()],
        )
        .unwrap();

        // Simulate eval_tool_calls's dedup: it keeps the LAST call for
        // each duplicate id, so the executor runs dup_2 and unique.
        let results = vec![
            ToolResult::new(dup_2, json!({"stdout": "ls-output"})),
            ToolResult::new(unique, json!({"stdout": "hi"})),
        ];
        super::add_tool_results(&mut session, &results).unwrap();

        // In-memory state should have exactly 2 slots — no orphan pending.
        let last = session.messages.last().unwrap();
        let MessageContent::ToolCalls(tc) = &last.content else {
            panic!("expected ToolCalls content");
        };
        assert_eq!(
            tc.tool_results.len(),
            2,
            "pending slots should be deduped to match eval_tool_calls"
        );
        for slot in &tc.tool_results {
            let output_str = slot.output.to_string();
            assert!(
                !output_str.contains("tool response pending"),
                "no slot should retain the pending placeholder, got: {output_str}"
            );
        }

        // The on-disk log must not contain the pending-placeholder string.
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        assert!(
            !content.contains("tool response pending"),
            "log should never persist pending-placeholder outputs, got:\n{content}"
        );

        // And the tool_results entry should contain two unique ids, not three.
        let dup_id_occurrences = content.matches("toolu_dup").count();
        assert_eq!(
            dup_id_occurrences, 2,
            "toolu_dup should appear once in tool_calls and once in tool_results (not more)"
        );
    }

    /// Verify that a session file with an orphan `tool_calls` entry
    /// (process crashed mid-round) is repaired on load by
    /// synthesizing lost-response error outputs for every pending
    /// call.
    #[test]
    fn load_repairs_orphan_tool_calls_at_eof() {
        use crate::tool::ToolCall;
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(&global_config, "hello", Some(agent));

        let call = ToolCall {
            name: "my_tool".to_string(),
            arguments: json!({"x": 1}),
            id: Some("c1".to_string()),
            thought_signature: None,
        };
        super::add_tool_calls(&mut session, &input, "calling tool", None, &[call]).unwrap();
        // Deliberately do NOT call add_tool_results — simulates a
        // crash mid-round.

        // Parse the log directly (same path as super::load, minus
        // model resolution which needs a fully-configured catalog).
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        let reloaded = super::load_from_log_for_test(&content);

        let last = reloaded
            .messages
            .last()
            .expect("session should have messages");
        assert_eq!(last.role, MessageRole::Tool);
        let MessageContent::ToolCalls(tc) = &last.content else {
            panic!("expected ToolCalls content");
        };
        assert_eq!(tc.tool_results.len(), 1);
        let output_str = tc.tool_results[0].output.to_string();
        assert!(
            output_str.contains("tool response lost"),
            "expected synthesized lost-response error, got: {output_str}"
        );
    }

    /// Regression test for #390 (orphan-repair path): after a crash
    /// mid-tool-round the session is repaired on reload (orphan tool
    /// calls get a synthesised "lost" result so the tail is a proper
    /// `Tool` message).  A fresh user prompt sent to that repaired
    /// session must still be included in `build_messages` — not dropped
    /// because the session tail happens to be `Tool`.
    #[test]
    fn fresh_message_after_orphan_repair_is_included_in_build_messages() {
        use crate::tool::ToolCall;
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(&global_config, "first query", Some(agent));

        // Write tool_calls but NOT tool_results — simulates crash mid-round.
        let call = ToolCall {
            name: "Bash".to_string(),
            arguments: json!({"command": "ls"}),
            id: Some("orphan_c1".to_string()),
            thought_signature: None,
        };
        super::add_tool_calls(&mut session, &input, "running bash", None, &[call]).unwrap();

        // Reload — orphan repair synthesises a lost-response Tool tail.
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        let repaired = super::load_from_log_for_test(&content);
        assert_eq!(
            repaired.messages.last().unwrap().role,
            MessageRole::Tool,
            "repaired session tail must be Tool"
        );

        // Fresh user prompt after resume — no tool_calls on the input.
        let fresh_input =
            crate::config::input::from_str(&global_config, "fresh prompt after crash", None);
        let messages = super::build_messages(&repaired, &fresh_input).unwrap();

        assert!(
            messages
                .iter()
                .filter(|m| m.role.is_user())
                .any(|m| m.content.to_text().contains("fresh prompt after crash")),
            "fresh user message must be included in build_messages after orphan repair; \
             got: {messages:#?}"
        );
    }

    /// Round-trip: write a full session (plain-text + tool round) and
    /// reload it through `load_from_log`.  Verify the in-memory
    /// messages are reconstructed correctly.
    #[test]
    fn append_message_persists_timestamp_and_reloads() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(&global_config, "hello", Some(agent));

        super::add_assistant_text(&mut session, &input, "world", None).unwrap();

        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        assert!(content.contains("timestamp:"));

        let reloaded = super::load_from_log_for_test(&content);
        let last = reloaded
            .messages
            .last()
            .expect("assistant message reloaded");
        assert!(last.log_timestamp.is_some());
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
    fn session_round_trips_through_load() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));

        let call = ToolCall {
            name: "search".to_string(),
            arguments: json!({"query": "test"}),
            id: Some("c1".to_string()),
            thought_signature: None,
        };
        let results = vec![ToolResult::new(
            call.clone(),
            json!({"results": ["a", "b"]}),
        )];

        let input1 =
            crate::config::input::from_str(&global_config, "find test", Some(agent.clone()));
        super::add_tool_calls(&mut session, &input1, "searching...", None, &[call]).unwrap();
        super::add_tool_results(&mut session, &results).unwrap();

        let input2 = crate::config::input::from_str(&global_config, "find test", Some(agent));
        super::add_assistant_text(&mut session, &input2, "found results", None).unwrap();

        // Parse the log directly (same path as super::load, minus
        // model resolution which needs a fully-configured catalog).
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        let reloaded = super::load_from_log_for_test(&content);

        assert_eq!(
            reloaded.messages.len(),
            session.messages.len(),
            "reloaded message count should match"
        );
        assert_eq!(
            reloaded.messages.last().unwrap().content.to_text(),
            "found results",
            "final reloaded message should preserve the last assistant output"
        );
        // The Tool message should have its outputs intact.
        let tool_msg = reloaded
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("session should contain a Tool message");
        let MessageContent::ToolCalls(tc) = &tool_msg.content else {
            panic!("expected ToolCalls content on the Tool message");
        };
        assert_eq!(
            tc.tool_results[0].output,
            json!({"results": ["a", "b"]}),
            "reloaded tool output should match what we wrote"
        );
    }

    #[test]
    fn legacy_tool_result_without_content_loads_with_empty_content() {
        use serde_json::json;

        let content = r#"---
type: header
model: test:model
agent: default
save_session: true
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
    fn session_serialization_omits_empty_tool_result_content() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));

        let call = ToolCall {
            name: "search".to_string(),
            arguments: json!({"query": "test"}),
            id: Some("c1".to_string()),
            thought_signature: None,
        };
        let result = ToolResult::new(call.clone(), json!({"results": ["a", "b"]}));

        let input = crate::config::input::from_str(&global_config, "find test", Some(agent));
        super::add_tool_calls(&mut session, &input, "searching...", None, &[call]).unwrap();
        super::add_tool_results(&mut session, &[result]).unwrap();

        let serialized = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        let tool_results_doc = serialized
            .split("---")
            .find(|doc| doc.contains("type: tool_results"))
            .expect("persisted session should contain tool_results document");
        assert!(
            !tool_results_doc.contains("content:"),
            "empty tool result content should be omitted from persisted tool_results YAML"
        );
    }

    #[test]
    fn tool_result_image_content_survives_add_tool_results_and_build_messages() {
        use crate::tool::{ToolCall, ToolResult};
        use harnx_core::message::{ImageUrl, MessageContentPart};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        // The session needs a path for cid: refs to be persisted
        session.set_sessions_dir(tmp.path().to_path_buf());
        let session_path = tmp
            .path()
            .join("test-session.yaml")
            .to_string_lossy()
            .to_string();
        session.path = Some(session_path);

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));
        let input = crate::config::input::from_str(&global_config, "inspect image", Some(agent));

        // Start with a data: URL - will be converted to cid: when persisted
        let data_uri = "data:image/png;base64,AAAA".to_string();
        let call = ToolCall {
            name: "read".to_string(),
            arguments: json!({"path": "chart.png"}),
            id: Some("c1".to_string()),
            thought_signature: None,
        };
        let result = ToolResult {
            call: call.clone(),
            output: json!({
                "content": [{
                    "type": "image",
                    "mime_type": "image/png",
                    "data": "<image: image/png, 4 base64 chars>"
                }]
            }),
            content: vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: data_uri.clone(),
                },
            }],
            switch_agent: None,
        };

        super::add_tool_calls(&mut session, &input, "reading...", None, &[call]).unwrap();
        super::add_tool_results(&mut session, &[result]).unwrap();

        // build_messages no longer expands cid: refs - that's now done in
        // prepare_completion_data based on client capability
        let messages = super::build_messages(&session, &input).unwrap();
        let tool_message = messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("tool message should be present");
        let MessageContent::ToolCalls(tool_calls) = &tool_message.content else {
            panic!("expected tool-call message, got {tool_message:#?}");
        };
        assert_eq!(tool_calls.tool_results.len(), 1);
        assert_eq!(tool_calls.tool_results[0].content.len(), 1);
        // The URL will be a cid: ref since the message was persisted to disk
        match &tool_calls.tool_results[0].content[0] {
            MessageContentPart::ImageUrl { image_url } => {
                // After persistence, data: URLs become cid: refs
                assert!(
                    image_url.url.starts_with("cid:"),
                    "Expected cid: ref, got {}",
                    image_url.url
                );
            }
            other => panic!("expected ImageUrl content part, got {other:#?}"),
        }
        assert_eq!(
            tool_calls.tool_results[0].output,
            json!({
                "content": [{
                    "type": "image",
                    "mime_type": "image/png",
                    "data": "<image: image/png, 4 base64 chars>"
                }]
            })
        );
    }

    #[test]
    fn session_persistence_round_trips_tool_result_image_content_with_redacted_output() {
        use crate::tool::{ToolCall, ToolResult};
        use harnx_core::message::{ImageUrl, MessageContentPart};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));

        let base64_payload = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(8);
        let data_uri = format!("data:image/png;base64,{base64_payload}");
        let call = ToolCall {
            name: "read".to_string(),
            arguments: json!({"path": "chart.png"}),
            id: Some("c1".to_string()),
            thought_signature: None,
        };
        let redacted_output = json!({
            "content": [{
                "type": "image",
                "mime_type": "image/png",
                "data": format!("<image: image/png, {} base64 chars>", base64_payload.len())
            }]
        });
        let result = ToolResult {
            call: call.clone(),
            output: redacted_output.clone(),
            content: vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: data_uri.clone(),
                },
            }],
            switch_agent: None,
        };

        let input = crate::config::input::from_str(&global_config, "inspect image", Some(agent));
        super::add_tool_calls(&mut session, &input, "reading...", None, &[call]).unwrap();
        super::add_tool_results(&mut session, &[result]).unwrap();

        let persisted = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        assert!(persisted.contains("<image: image/png,"));
        assert!(
            persisted.contains("cid:"),
            "tool-result image is referenced by cid"
        );
        assert!(
            !persisted.contains(&base64_payload),
            "tool-result base64 must not be inlined in the transcript"
        );
        let dir = crate::config::attachments::attachments_dir_for(std::path::Path::new(
            session.path.as_ref().unwrap(),
        ));
        assert_eq!(std::fs::read_dir(&dir).unwrap().flatten().count(), 1);

        let reloaded = super::load_from_log_for_test(&persisted);
        let MessageContent::ToolCalls(tool_calls) = &reloaded
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("reloaded tool message should be present")
            .content
        else {
            panic!("expected reloaded tool-call message");
        };
        assert_eq!(tool_calls.tool_results.len(), 1);
        assert_eq!(tool_calls.tool_results[0].content.len(), 1);
        match &tool_calls.tool_results[0].content[0] {
            MessageContentPart::ImageUrl { image_url } => {
                assert!(
                    image_url.url.starts_with("cid:"),
                    "reloaded content keeps cid ref"
                );
            }
            other => panic!("expected ImageUrl content part, got {other:#?}"),
        }
        assert_eq!(tool_calls.tool_results[0].output, redacted_output);
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
    fn load_tracks_log_entry_count_and_append_event_increments_next_seq() {
        use tempfile::TempDir;

        let content = r#"type: header
model: test
---
type: message
role: user
content: first
---
type: message
role: assistant
content: second
"#;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.yaml");
        std::fs::write(&path, content).unwrap();

        let mut session = super::load_from_log_for_test(content);
        session.path = Some(path.display().to_string());

        assert_eq!(session.log_entry_count, 3);
        assert_eq!(session.next_seq(), 3);

        let appended = super::append_event(
            &mut session,
            &SessionLogEntry::Message {
                id: None,
                timestamp: None,
                fence_token: None,
                role: MessageRole::User,
                content: MessageContent::Text("third".to_string()),
            },
        );

        assert!(appended);
        assert_eq!(session.log_entry_count, 4);
        assert_eq!(session.next_seq(), 4);
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

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

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

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

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
    fn save_externalizes_inline_image_content() {
        use harnx_core::message::{ImageUrl, MessageContent, MessageContentPart};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let base64_payload = "QUJDREVG".repeat(64);
        let data_uri = format!("data:image/png;base64,{base64_payload}");
        session.messages.push(Message::new(
            MessageRole::User,
            MessageContent::Array(vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl { url: data_uri },
            }]),
        ));

        let path = tmp.path().join("s1.yaml");
        super::save(&mut session, "s1", &path, false).unwrap();

        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(
            persisted.contains("cid:"),
            "saved transcript references a cid"
        );
        assert!(
            !persisted.contains(&base64_payload),
            "no inline base64 in saved transcript"
        );
        let dir = crate::config::attachments::attachments_dir_for(&path);
        assert_eq!(std::fs::read_dir(&dir).unwrap().flatten().count(), 1);
    }

    #[test]
    fn externalize_content_rewrites_image_to_cid_and_writes_file() {
        use harnx_core::message::{ImageUrl, MessageContent, MessageContentPart};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());
        super::ensure_log_file(&mut session);
        assert!(session.path.is_some(), "ensure_log_file should set a path");

        let base64_payload = "QUJDREVG".repeat(64);
        let data_uri = format!("data:image/png;base64,{base64_payload}");
        let mut content = MessageContent::Array(vec![
            MessageContentPart::Text {
                text: "look".into(),
            },
            MessageContentPart::ImageUrl {
                image_url: ImageUrl { url: data_uri },
            },
        ]);

        let map = super::session_externalize::externalize_content(&session, &mut content);
        assert_eq!(map.len(), 1, "one cid recorded");
        assert!(
            map.keys().next().unwrap().starts_with("cid:"),
            "map is keyed by the cid reference"
        );

        match &content {
            MessageContent::Array(parts) => match &parts[1] {
                MessageContentPart::ImageUrl { image_url } => {
                    assert!(image_url.url.starts_with("cid:"));
                    assert!(!image_url.url.contains(&base64_payload));
                }
                other => panic!("expected ImageUrl, got {other:#?}"),
            },
            other => panic!("expected Array, got {other:#?}"),
        }

        let dir = crate::config::attachments::attachments_dir_for(std::path::Path::new(
            session.path.as_ref().unwrap(),
        ));
        assert_eq!(std::fs::read_dir(&dir).unwrap().flatten().count(), 1);
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
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert!(session.messages[0].content.to_text().contains("SUMMARY"));
        assert!(session.messages[0].content.to_text().contains("sys"));
        assert_eq!(session.messages[1].content.to_text(), "recent u");
        assert_eq!(session.messages[2].content.to_text(), "recent a");
    }
}
