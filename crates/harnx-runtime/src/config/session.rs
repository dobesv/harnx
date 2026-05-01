use super::input::*;
use super::*;

pub use harnx_core::session::{AutoName, Session, SessionLogEntry};

use crate::client::{
    render_message_input, CompletionTokenUsage, Message, MessageContent, MessageRole,
};
use harnx_render::MarkdownRender;

use anyhow::{Context, Result};
use fancy_regex::Regex;
use serde::Deserialize;
use std::fs::{read_to_string, write, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::LazyLock;

static RE_AUTONAME_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{8}T\d{6}-").unwrap());

pub fn new(config: &Config, name: &str) -> Result<Session> {
    let agent = config.extract_agent();
    let mut session = Session {
        name: name.to_string(),
        save_session: config.save_session,
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
        let mut session = new(config, name)?;
        apply_name_and_path(&mut session, name, path, config)?;
        session
    };

    Ok(session)
}

fn load_from_log(config: &Config, name: &str, path: &Path, content: &str) -> Result<Session> {
    let raw_entries = collect_raw_log_entries(content, name)?;
    let mut session = replay_log_entries(&raw_entries, name)?;
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
}

fn collect_raw_log_entries(content: &str, name: &str) -> Result<Vec<(usize, SessionLogEntry)>> {
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
    let mut effective_entries = Vec::new();

    for (seq, entry) in raw_entries {
        match entry {
            SessionLogEntry::Rewind { after_seq } => {
                if *after_seq >= *seq {
                    log::warn!(
                        "Skipping rewind entry #{seq} in session {name}: after_seq {after_seq} must be less than current seq"
                    );
                    continue;
                }
                if !effective_entries
                    .iter()
                    .any(|(existing_seq, _)| existing_seq == after_seq)
                {
                    log::warn!(
                        "Skipping rewind entry #{seq} in session {name}: after_seq {after_seq} not present in replay state"
                    );
                    continue;
                }
                effective_entries.retain(|(existing_seq, _)| *existing_seq <= *after_seq);
            }
            SessionLogEntry::EditEntries {
                from,
                to,
                replacements,
            } => {
                if from > to {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: invalid range [{from}, {to}]"
                    );
                    continue;
                }
                if *to >= *seq {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: range [{from}, {to}] must reference earlier entries only"
                    );
                    continue;
                }

                let Some(start_idx) = effective_entries
                    .iter()
                    .position(|(existing_seq, _)| existing_seq == from)
                else {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: from seq {from} not present in replay state"
                    );
                    continue;
                };
                let Some(end_idx) = effective_entries
                    .iter()
                    .position(|(existing_seq, _)| existing_seq == to)
                else {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: to seq {to} not present in replay state"
                    );
                    continue;
                };
                if start_idx > end_idx {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: range [{from}, {to}] not in replay order"
                    );
                    continue;
                }
                if effective_entries[start_idx..=end_idx]
                    .iter()
                    .any(|(existing_seq, _)| *existing_seq < *from || *existing_seq > *to)
                {
                    log::warn!(
                        "Skipping edit_entries entry #{seq} in session {name}: range [{from}, {to}] is not contiguous in replay state"
                    );
                    continue;
                }

                let parsed_replacements: Vec<_> = replacements
                    .iter()
                    .enumerate()
                    .filter_map(|(replacement_idx, replacement)| {
                        match serde_yaml::from_str::<SessionLogEntry>(replacement) {
                            Ok(parsed) => {
                                // Replacements inherit EditEntries seq because originals are
                                // logically removed from effective stream. Future rewind/edit
                                // operations must target mutation seq, not replaced seqs.
                                Some((*seq, parsed))
                            },
                            Err(err) => {
                                log::warn!(
                                    "Skipping replacement #{replacement_idx} in edit_entries entry #{seq} for session {name}: {err}"
                                );
                                None
                            }
                        }
                    })
                    .collect();

                effective_entries.splice(start_idx..=end_idx, parsed_replacements);
            }
            SessionLogEntry::Unknown => {}
            _ => effective_entries.push((*seq, entry.clone())),
        }
    }

    effective_entries
}

fn replay_log_entries(raw_entries: &[(usize, SessionLogEntry)], name: &str) -> Result<Session> {
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
                session.agent_variables = agent_variables;
                session.agent_instructions = agent_instructions;
                session.model_fallbacks = model_fallbacks;
                session.compaction_agent = compaction_agent;
            }
            SessionLogEntry::Message { role, content } => {
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
                session
                    .messages
                    .push(Message::new(role, content).with_log_seq(seq));
            }
            SessionLogEntry::ToolCalls {
                text,
                thought,
                calls,
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
                });
            }
            SessionLogEntry::ToolResults { results } => {
                let Some(PendingToolCalls {
                    seq,
                    text,
                    thought,
                    calls,
                }) = pending.take()
                else {
                    anyhow::bail!(
                        "Invalid log entry in session {name}: tool_results without a                          preceding tool_calls entry"
                    );
                };
                session
                    .messages
                    .push(assemble_tool_message(text, thought, calls, results).with_log_seq(seq));
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
fn load_from_log_for_test(content: &str) -> Session {
    let raw_entries = collect_raw_log_entries(content, "test").expect("valid log entries");
    let mut session = replay_log_entries(&raw_entries, "test").expect("replay should succeed");
    session.log_entry_count = raw_entries.len();
    session
}

fn repair_orphan_tool_calls(pending: PendingToolCalls, _name: &str) -> Result<Message> {
    let PendingToolCalls {
        seq,
        text,
        thought,
        calls,
    } = pending;
    let lost = harnx_core::session::ToolOutput {
        id: None,
        name: String::new(),
        output: serde_json::json!({
            "error": "tool response lost (session was interrupted before results were persisted)"
        }),
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
    Ok(assemble_tool_message(text, thought, calls, results).with_log_seq(seq))
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

fn apply_name_and_path(
    session: &mut Session,
    name: &str,
    path: &Path,
    config: &Config,
) -> Result<()> {
    if let Some(autoname) = name.strip_prefix("_/") {
        session.name = TEMP_SESSION_NAME.to_string();
        session.path = Some(path.display().to_string());
        if let Ok(true) = RE_AUTONAME_PREFIX.is_match(autoname) {
            session.autoname = Some(AutoName::new(autoname[16..].to_string()));
        }
    } else {
        session.name = name.to_string();
        session.path = Some(path.display().to_string());
    }

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

    let (dir, session_name) = resolve_save_path(session, &sessions_dir);
    let session_path = dir.join(format!("{session_name}.yaml"));
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
    ensure_log_file(session);
    let Some(path_str) = &session.path else {
        return false;
    };
    let path = Path::new(path_str);
    let Ok(yaml) = serde_yaml::to_string(entry) else {
        return false;
    };
    let mut data = String::from("---\n");
    data.push_str(&yaml);
    let Ok(mut file) = OpenOptions::new().append(true).open(path) else {
        return false;
    };
    if file.write_all(data.as_bytes()).is_ok() {
        session.log_entry_count += 1;
        true
    } else {
        false
    }
}

pub fn resolve_save_path(session: &mut Session, session_dir: &Path) -> (PathBuf, String) {
    if let Some((dir, name)) = session.resolved_save_name.clone() {
        // Update the cached name with autoname if it arrived since
        // the first resolution.
        if session.name == TEMP_SESSION_NAME && !name.contains('-') {
            if let Some(autoname) = session.autoname() {
                let name = format!("{name}-{autoname}");
                session.resolved_save_name = Some((dir.clone(), name.clone()));
                return (dir, name);
            }
        }
        return (dir, name);
    }
    let mut dir = session_dir.to_path_buf();
    let mut name = session.name.clone();
    if name == TEMP_SESSION_NAME {
        dir = dir.join("_");
        let now = chrono::Local::now();
        name = now.format("%Y%m%dT%H%M%S").to_string();
        if let Some(autoname) = session.autoname() {
            name = format!("{name}-{autoname}");
        }
    }
    session.resolved_save_name = Some((dir.clone(), name.clone()));
    (dir, name)
}

pub fn render(
    session: &Session,
    render: &mut MarkdownRender,
    agent_info: &Option<(String, Vec<String>)>,
) -> Result<String> {
    let mut items = vec![];

    if let Some(path) = &session.path {
        items.push(("path", path.to_string()));
    }

    if let Some(autoname) = session.autoname() {
        items.push(("autoname", autoname.to_string()));
    }

    items.push(("model", session.model().id()));

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

    let mut lines: Vec<String> = items
        .iter()
        .map(|(name, value)| format!("{name:<20}{value}"))
        .collect();

    lines.push(String::new());

    if !session.is_empty() {
        let resolve_url_fn = |url: &str| resolve_data_url(&session.data_urls, url.to_string());

        for message in &session.messages {
            match message.role {
                MessageRole::System => {
                    lines.push(render.render(&render_message_input(
                        &message.content,
                        resolve_url_fn,
                        agent_info,
                    )));
                }
                MessageRole::Assistant => {
                    if let MessageContent::Text(text) = &message.content {
                        lines.push(render.render(text));
                    }
                    lines.push("".into());
                }
                MessageRole::User => {
                    lines.push(format!(
                        ">> {}",
                        render_message_input(&message.content, resolve_url_fn, agent_info)
                    ));
                }
                MessageRole::Tool => {
                    lines.push(render_message_input(
                        &message.content,
                        resolve_url_fn,
                        agent_info,
                    ));
                }
            }
        }
    }

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
    let (session_dir, session_name) = resolve_save_path(session, session_dir);
    let session_path = session_dir.join(format!("{session_name}.yaml"));
    save(session, &session_name, &session_path, is_tui)?;
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

    // Write in the new log format.
    let mut content = serde_yaml::to_string(&session.build_header_entry())
        .with_context(|| format!("Failed to serialize session header for '{}'", session.name))?;
    for msg in &session.compressed_messages {
        let entry = SessionLogEntry::Message {
            role: msg.role,
            content: msg.content.clone(),
        };
        content.push_str("---\n");
        content.push_str(
            &serde_yaml::to_string(&entry)
                .with_context(|| format!("Failed to serialize message in '{}'", session.name))?,
        );
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
                    format!("Failed to serialize compress entry in '{}'", session.name)
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
            let entry = SessionLogEntry::Message {
                role: msg.role,
                content: msg.content.clone(),
            };
            content.push_str("---\n");
            content.push_str(
                &serde_yaml::to_string(&entry).with_context(|| {
                    format!("Failed to serialize message in '{}'", session.name)
                })?,
            );
        }
    } else {
        for msg in &session.messages {
            let entry = SessionLogEntry::Message {
                role: msg.role,
                content: msg.content.clone(),
            };
            content.push_str("---\n");
            content.push_str(
                &serde_yaml::to_string(&entry).with_context(|| {
                    format!("Failed to serialize message in '{}'", session.name)
                })?,
            );
        }
    }
    if !session.data_urls.is_empty() {
        let entry = SessionLogEntry::DataUrls {
            urls: session.data_urls.clone(),
        };
        content.push_str("---\n");
        content.push_str(
            &serde_yaml::to_string(&entry)
                .with_context(|| format!("Failed to serialize data_urls in '{}'", session.name))?,
        );
    }

    write(session_path, &content).with_context(|| {
        format!(
            "Failed to write session '{}' to '{}'",
            session.name,
            session_path.display()
        )
    })?;

    if is_tui {
        crate::utils::emit_info(format!(
            "✓ Saved the session to '{}'.",
            session_path.display()
        ));
    }

    if session.name() != session_name {
        session.name = session_name.to_string()
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
        let assistant_msg = Message::new(MessageRole::Assistant, content).with_log_seq(seq);
        all_appended &= append_event(
            session,
            &SessionLogEntry::Message {
                role: assistant_msg.role,
                content: assistant_msg.content.clone(),
            },
        );
        session.messages.push(assistant_msg);
        session.dirty = !all_appended;
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
    all_appended &= append_event(
        session,
        &SessionLogEntry::ToolCalls {
            text: output.to_string(),
            thought: thought.map(str::to_string),
            calls: calls.clone(),
        },
    );
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
    session
        .messages
        .push(Message::new(MessageRole::Tool, content).with_log_seq(tool_calls_seq));
    session.dirty = !all_appended;
    session.update_tokens();
    Ok(())
}

/// Finalize the tool round opened by [`add_tool_calls`] by filling in
/// the in-memory outputs and writing a `ToolResults` log entry.
/// Matches each result to its call by id (or by position when the id
/// is absent).
pub fn add_tool_results(session: &mut Session, results: &[crate::tool::ToolResult]) -> Result<()> {
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
            slot.switch_agent = replacement.switch_agent;
        }
    }

    let log_results: Vec<harnx_core::session::ToolOutput> = pending
        .tool_results
        .iter()
        .map(|r| harnx_core::session::ToolOutput {
            id: r.call.id.clone(),
            name: r.call.name.clone(),
            output: r.output.clone(),
            switch_agent: r.switch_agent.clone(),
        })
        .collect();

    let appended = append_event(
        session,
        &SessionLogEntry::ToolResults {
            results: log_results,
        },
    );
    if !appended {
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

/// Shared round-opening setup used by both `add_assistant_text` and
/// `add_tool_calls`: first-turn agent-message injection, user-message
/// push (skipped on continuation rounds), data-URL persistence, and
/// any queued injected-user-text.  Returns `true` iff every log
/// append succeeded.
fn begin_turn(session: &mut Session, input: &Input, output: &str) -> Result<bool> {
    let mut all_appended = true;
    let is_continuation = is_tool_continuation(input, &session.messages);
    if session.messages.is_empty() {
        if session.name == TEMP_SESSION_NAME && session.save_session != Some(false) {
            let raw_input = input.raw();
            let chat_history = format!("USER: {raw_input}\nASSISTANT: {output}\n");
            session.autoname = Some(AutoName::new_from_chat_history(chat_history));
        }
        let agent_messages = input.agent().build_messages(input)?;
        for msg in agent_messages {
            let seq = session.next_seq();
            all_appended &= append_event(
                session,
                &SessionLogEntry::Message {
                    role: msg.role,
                    content: msg.content.clone(),
                },
            );
            session.messages.push(msg.with_log_seq(seq));
        }
    } else if !is_continuation {
        let seq = session.next_seq();
        let user_msg = Message::new(MessageRole::User, input.message_content()).with_log_seq(seq);
        all_appended &= append_event(
            session,
            &SessionLogEntry::Message {
                role: user_msg.role,
                content: user_msg.content.clone(),
            },
        );
        session.messages.push(user_msg);
    }
    let new_data_urls = input.data_urls();
    if !new_data_urls.is_empty() {
        all_appended &= append_event(
            session,
            &SessionLogEntry::DataUrls {
                urls: new_data_urls.clone(),
            },
        );
    }
    session.data_urls.extend(new_data_urls);
    if let Some(injected) = input.injected_user_text() {
        let seq = session.next_seq();
        let injected_msg = Message::new(
            MessageRole::User,
            MessageContent::Text(injected.to_string()),
        )
        .with_log_seq(seq);
        all_appended &= append_event(
            session,
            &SessionLogEntry::Message {
                role: injected_msg.role,
                content: injected_msg.content.clone(),
            },
        );
        session.messages.push(injected_msg);
    }
    Ok(all_appended)
}

pub fn clear_messages(session: &mut Session) {
    session.messages.clear();
    session.compressed_messages.clear();
    session.data_urls.clear();
    session.autoname = None;
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

pub fn build_messages(session: &Session, input: &Input) -> Result<Vec<Message>> {
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
        new(&Config::default(), "test").unwrap()
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
        assert!(matches!(seqs[1], (1, SessionLogEntry::Message { .. })));
        assert!(matches!(
            seqs[2],
            (2, SessionLogEntry::Rewind { after_seq: 1 })
        ));
        assert!(matches!(
            seqs[3],
            (3, SessionLogEntry::EditEntries { from: 1, to: 1, .. })
        ));
        assert!(matches!(seqs[4], (4, SessionLogEntry::Message { .. })));

        let session = super::load_from_log_for_test(content);
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content.to_text(), "second");
        assert_eq!(session.log_entry_count, 5);
        assert_eq!(session.next_seq(), 5);
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
        use harnx_render::{MarkdownRender, RenderOptions};

        let mut session = test_session();
        session.set_model_fallbacks(vec![
            "anthropic:claude".to_string(),
            "google:gemini".to_string(),
        ]);

        let options = RenderOptions::default();
        let mut md_render = MarkdownRender::init(options).unwrap();
        let agent_info: Option<(String, Vec<String>)> = None;
        let output = super::render(&session, &mut md_render, &agent_info).unwrap();

        assert!(
            output.contains("model_fallbacks"),
            "render output should contain model_fallbacks key: {output}"
        );
        assert!(
            output.contains("anthropic:claude,google:gemini"),
            "render output should contain comma-separated fallback values: {output}"
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
}
