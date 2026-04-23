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

pub fn new(config: &Config, name: &str) -> Session {
    let agent = config.extract_agent();
    let mut session = Session {
        name: name.to_string(),
        save_session: config.save_session,
        ..Default::default()
    };
    session.set_agent(&agent);
    session.dirty = false;
    session
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
        let mut session = new(config, name);
        apply_name_and_path(&mut session, name, path, config);
        session
    };

    Ok(session)
}

fn load_from_log(config: &Config, name: &str, path: &Path, content: &str) -> Result<Session> {
    let mut session = Session::default();

    // Pending ToolCalls entry awaiting a matching ToolResults entry.
    // On any other entry (or EOF) while pending, we repair by
    // synthesizing lost-response errors for each pending call — this
    // only matters for the tail of the log (crash mid tool round);
    // mid-log corruption would be an invariant violation.
    let mut pending: Option<PendingToolCalls> = None;

    for document in serde_yaml::Deserializer::from_str(content) {
        let entry = SessionLogEntry::deserialize(document)
            .with_context(|| format!("Invalid log entry in session {name}"))?;
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
                    bail!(
                        "Invalid log entry in session {name}: Tool-role Message entries are \
                         no longer supported; use tool_calls/tool_results entries"
                    );
                }
                session.messages.push(Message::new(role, content));
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
                    text,
                    thought,
                    calls,
                });
            }
            SessionLogEntry::ToolResults { results } => {
                let Some(PendingToolCalls {
                    text,
                    thought,
                    calls,
                }) = pending.take()
                else {
                    bail!(
                        "Invalid log entry in session {name}: tool_results without a \
                         preceding tool_calls entry"
                    );
                };
                session
                    .messages
                    .push(assemble_tool_message(text, thought, calls, results));
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
        }
    }

    // EOF with an orphan ToolCalls: the process was interrupted
    // between dispatching tools and persisting their results. Repair
    // so we can still replay a valid alternating user/assistant
    // sequence to the model.
    if let Some(pending) = pending.take() {
        session
            .messages
            .push(repair_orphan_tool_calls(pending, name)?);
    }

    session.model =
        crate::client::retrieve_model(&config.clients, &session.model_id, ModelType::Chat)?;
    apply_name_and_path(&mut session, name, path, config);
    session.update_tokens();
    Ok(session)
}

struct PendingToolCalls {
    text: String,
    thought: Option<String>,
    calls: Vec<crate::tool::ToolCall>,
}

/// Test-only log parser — runs the full load pipeline (including the
/// orphan-ToolCalls repair pass) but skips the model-catalog lookup
/// that `super::load` performs, so it works against the minimal
/// `Config::default` used in unit tests.
#[cfg(test)]
fn load_from_log_for_test(content: &str) -> Session {
    let mut session = Session::default();
    let mut pending: Option<PendingToolCalls> = None;
    for document in serde_yaml::Deserializer::from_str(content) {
        let entry = SessionLogEntry::deserialize(document).expect("valid log entry");
        match entry {
            SessionLogEntry::Header { .. } => {}
            SessionLogEntry::Message { role, content } => {
                if let Some(pending) = pending.take() {
                    session
                        .messages
                        .push(repair_orphan_tool_calls(pending, "test").unwrap());
                }
                assert_ne!(role, MessageRole::Tool, "legacy Tool Message unsupported");
                session.messages.push(Message::new(role, content));
            }
            SessionLogEntry::ToolCalls {
                text,
                thought,
                calls,
            } => {
                if let Some(pending) = pending.take() {
                    session
                        .messages
                        .push(repair_orphan_tool_calls(pending, "test").unwrap());
                }
                pending = Some(PendingToolCalls {
                    text,
                    thought,
                    calls,
                });
            }
            SessionLogEntry::ToolResults { results } => {
                let pending = pending.take().expect("tool_results must follow tool_calls");
                session.messages.push(assemble_tool_message(
                    pending.text,
                    pending.thought,
                    pending.calls,
                    results,
                ));
            }
            SessionLogEntry::DataUrls { urls } => session.data_urls.extend(urls),
            SessionLogEntry::Compress { prompt } => {
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
        }
    }
    if let Some(pending) = pending.take() {
        session
            .messages
            .push(repair_orphan_tool_calls(pending, "test").unwrap());
    }
    session
}

fn repair_orphan_tool_calls(pending: PendingToolCalls, _name: &str) -> Result<Message> {
    let PendingToolCalls {
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
    Ok(assemble_tool_message(text, thought, calls, results))
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

fn apply_name_and_path(session: &mut Session, name: &str, path: &Path, config: &Config) {
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
            session.agent_prompt = agent.interpolated_instructions();
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
    file.write_all(data.as_bytes()).is_ok()
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

    write(session_path, content).with_context(|| {
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

    session.dirty = false;

    Ok(())
}

pub fn to_agent(session: &Session) -> Agent {
    Agent::new(session.to_agent_config())
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
        let mut all_appended = begin_turn(session, input, output);
        let content = match thought {
            Some(v) => MessageContent::Text(format!("<think>\n{v}\n</think>\n{output}")),
            _ => MessageContent::Text(output.to_string()),
        };
        let assistant_msg = Message::new(MessageRole::Assistant, content);
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
    let mut all_appended = begin_turn(session, input, output);
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
        .push(Message::new(MessageRole::Tool, content));
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

/// Shared round-opening setup used by both `add_assistant_text` and
/// `add_tool_calls`: first-turn agent-message injection, user-message
/// push (skipped on continuation rounds), data-URL persistence, and
/// any queued injected-user-text.  Returns `true` iff every log
/// append succeeded.
fn begin_turn(session: &mut Session, input: &Input, output: &str) -> bool {
    let mut all_appended = true;
    // Detect continuation rounds: if the last saved message is a Tool
    // message, we're continuing after tool execution and should NOT
    // add a duplicate user message.
    let is_continuation = session
        .messages
        .last()
        .is_some_and(|m| m.role == MessageRole::Tool);
    if session.messages.is_empty() {
        if session.name == TEMP_SESSION_NAME && session.save_session != Some(false) {
            let raw_input = input.raw();
            let chat_history = format!("USER: {raw_input}\nASSISTANT: {output}\n");
            session.autoname = Some(AutoName::new_from_chat_history(chat_history));
        }
        let agent_messages = input.agent().build_messages(input);
        for msg in &agent_messages {
            all_appended &= append_event(
                session,
                &SessionLogEntry::Message {
                    role: msg.role,
                    content: msg.content.clone(),
                },
            );
        }
        session.messages.extend(agent_messages);
    } else if !is_continuation {
        let user_msg = Message::new(MessageRole::User, input.message_content());
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
        let injected_msg = Message::new(
            MessageRole::User,
            MessageContent::Text(injected.to_string()),
        );
        all_appended &= append_event(
            session,
            &SessionLogEntry::Message {
                role: injected_msg.role,
                content: injected_msg.content.clone(),
            },
        );
        session.messages.push(injected_msg);
    }
    all_appended
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
    let messages = build_messages(session, input);
    serde_yaml::to_string(&messages).unwrap_or_else(|_| "Unable to echo message".into())
}

pub fn build_messages(session: &Session, input: &Input) -> Vec<Message> {
    let mut messages = session.messages.clone();
    if input.continue_output().is_some() {
        return messages;
    } else if input.regenerate() {
        while let Some(last) = messages.last() {
            if !last.role.is_user() {
                messages.pop();
            } else {
                break;
            }
        }
        return messages;
    }
    let mut need_add_msg = true;
    let len = messages.len();
    if len == 0 {
        messages = input.agent().build_messages(input);
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
    if need_add_msg {
        // When the agent was swapped after construction (e.g. compaction),
        // inject_system_prompt is true and we must prepend the agent's
        // system prompt — session messages won't already contain it.
        // On normal session turns the system prompt was stored on turn 1
        // by save_message(), so inject_system_prompt stays false.
        if input.inject_system_prompt() {
            let system_text = input.agent().system_text();
            if !system_text.is_empty() {
                messages.insert(
                    0,
                    Message::new(MessageRole::System, MessageContent::Text(system_text)),
                );
            }
        }
        messages.push(Message::new(MessageRole::User, input.message_content()));
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> Session {
        new(&Config::default(), "test")
    }

    #[test]
    fn set_agent_to_agent_round_trip_preserves_model_fallbacks() {
        let agent = Agent::new(AgentConfig::from_markdown(
            "test",
            "---\nmodel: openai:gpt-4o\nmodel_fallbacks:\n  - anthropic:claude\n  - google:gemini\n---\nYou are a test agent.",
        ));
        let mut session = test_session();

        session.set_agent(&agent);
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
        let input2 = crate::config::input::from_str(&global_config, "hello", Some(agent));
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
}
