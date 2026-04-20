use super::input::*;
use super::*;

pub use harnx_core::session::{AutoName, Session, SessionLogEntry};

use crate::client::{
    render_message_input, CompletionTokenUsage, Message, MessageContent, MessageRole,
};
use crate::render::MarkdownRender;

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
                session.messages.push(Message::new(role, content));
            }
            SessionLogEntry::DataUrls { urls } => {
                session.data_urls.extend(urls);
            }
            SessionLogEntry::Compress { prompt } => {
                session.compressed_messages.append(&mut session.messages);
                session.messages.push(Message::new(
                    MessageRole::System,
                    MessageContent::Text(prompt),
                ));
            }
            SessionLogEntry::Clear => {
                session.messages.clear();
                session.compressed_messages.clear();
                session.data_urls.clear();
            }
        }
    }

    session.model =
        crate::client::retrieve_model(&config.clients, &session.model_id, ModelType::Chat)?;
    apply_name_and_path(&mut session, name, path, config);
    session.update_tokens();
    Ok(session)
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
                println!("✓ Session saved at '{path}'.");
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
        println!("✓ Saved the session to '{}'.", session_path.display());
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

/// Append a pre-built Tool message to the session log.
/// Used by the ACP server to persist tool results separately from
/// the main `add_message` flow.
pub fn append_tool_round(session: &mut Session, tool_msg: &Message) {
    if !append_event(
        session,
        &SessionLogEntry::Message {
            role: tool_msg.role,
            content: tool_msg.content.clone(),
        },
    ) {
        session.dirty = true;
    }
    session.messages.push(tool_msg.clone());
    session.update_tokens();
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

pub fn add_message(
    session: &mut Session,
    input: &Input,
    output: &str,
    thought: Option<&str>,
    tool_results: &[crate::tool::ToolResult],
) -> Result<()> {
    if input.continue_output().is_some() {
        if let Some(message) = session.messages.last_mut() {
            if let MessageContent::Text(text) = &mut message.content {
                *text = format!("{text}{output}");
            }
        }
        // Continue/regenerate are edits to the last message; mark dirty
        // so the full-save fallback can persist them. We don't append
        // because they modify an existing entry.
        session.dirty = true;
    } else if input.regenerate() {
        if let Some(message) = session.messages.last_mut() {
            if let MessageContent::Text(text) = &mut message.content {
                *text = output.to_string();
            }
        }
        session.dirty = true;
    } else {
        let mut all_appended = true;
        // Detect continuation rounds: if the last saved message is a Tool
        // message, we're continuing after tool execution and should NOT add
        // a duplicate user message.
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
            let agent_messages = crate::config::agent::build_messages(input.agent(), input);
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
        // Only process input.tool_calls() when this is NOT a
        // continuation round.  On continuation rounds the tool results
        // were already persisted incrementally via the `tool_results`
        // parameter in the previous round, so replaying them from the
        // merged input would create duplicates.
        if !is_continuation {
            if let Some(tool_calls) = input.tool_calls().clone() {
                let tool_msg =
                    Message::new(MessageRole::Tool, MessageContent::ToolCalls(tool_calls));
                all_appended &= append_event(
                    session,
                    &SessionLogEntry::Message {
                        role: tool_msg.role,
                        content: tool_msg.content.clone(),
                    },
                );
                session.messages.push(tool_msg);
            }
        }
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
        // Append tool results from this round (incremental persistence).
        if !tool_results.is_empty() {
            let tool_calls_content =
                MessageContent::ToolCalls(crate::client::MessageContentToolCalls::new(
                    tool_results.to_vec(),
                    output.to_string(),
                    thought.map(str::to_string),
                ));
            let tool_msg = Message::new(MessageRole::Tool, tool_calls_content);
            all_appended &= append_event(
                session,
                &SessionLogEntry::Message {
                    role: tool_msg.role,
                    content: tool_msg.content.clone(),
                },
            );
            session.messages.push(tool_msg);
        }
        // Only clear dirty if all events were appended; otherwise the
        // full-save fallback in exit() will persist the data.
        session.dirty = !all_appended;
    }
    session.update_tokens();
    Ok(())
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
        messages = crate::config::agent::build_messages(input.agent(), input);
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

    #[test]
    fn add_message_with_tool_results_saves_incrementally() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();

        // Round 1: user input + assistant output with tool calls
        let input = crate::config::input::from_str(
            &std::sync::Arc::new(parking_lot::RwLock::new(config.clone())),
            "hello",
            Some(agent.clone()),
        );
        let tool_results = vec![ToolResult::new(
            ToolCall {
                name: "test_tool".to_string(),
                arguments: json!({"arg": "val"}),
                id: Some("call_1".to_string()),
                thought_signature: None,
            },
            json!({"result": "ok"}),
        )];
        super::add_message(
            &mut session,
            &input,
            "I'll call a tool",
            None,
            &tool_results,
        )
        .unwrap();

        // Session should have: system/user msgs, assistant, tool
        assert!(
            session.messages.len() >= 3,
            "expected at least 3 messages (agent setup + assistant + tool), got {}",
            session.messages.len()
        );
        // Last message should be Tool role
        assert_eq!(
            session.messages.last().unwrap().role,
            MessageRole::Tool,
            "last message after tool round should be Tool role"
        );

        // The session file should exist and contain the intermediate state
        assert!(session.path.is_some(), "session file should be created");
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        assert!(
            content.contains("I'll call a tool"),
            "session file should contain assistant output from intermediate round"
        );
        assert!(
            content.contains("test_tool"),
            "session file should contain tool call info"
        );

        // Round 2: continuation (no new user msg), final assistant output
        let input2 = crate::config::input::from_str(
            &std::sync::Arc::new(parking_lot::RwLock::new(config)),
            "hello",
            Some(agent),
        );
        super::add_message(&mut session, &input2, "Here is the result", None, &[]).unwrap();

        // Should NOT have a duplicate user message
        let user_count = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count();
        assert_eq!(
            user_count, 1,
            "should have exactly 1 user message, not duplicates from continuation"
        );

        // Should have the final assistant message
        assert_eq!(
            session.messages.last().unwrap().role,
            MessageRole::Assistant
        );
        assert_eq!(
            session.messages.last().unwrap().content.to_text(),
            "Here is the result"
        );
    }

    /// Verify that when the continuation round's input carries merged
    /// tool_calls (from merge_tool_results), they don't create duplicate
    /// Tool messages — the tool results were already saved in round 1.
    #[test]
    fn continuation_with_merged_tool_calls_does_not_duplicate_tool_messages() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config));

        let tool_results = vec![ToolResult::new(
            ToolCall {
                name: "my_tool".to_string(),
                arguments: json!({"x": 1}),
                id: Some("call_1".to_string()),
                thought_signature: None,
            },
            json!("tool output"),
        )];

        // Round 1: save with tool_results — creates assistant + tool messages.
        let input1 = crate::config::input::from_str(&global_config, "hello", Some(agent.clone()));
        super::add_message(&mut session, &input1, "calling tool", None, &tool_results).unwrap();

        let tool_count_after_round1 = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .count();
        assert_eq!(
            tool_count_after_round1, 1,
            "round 1 should produce exactly 1 Tool message"
        );

        // Round 2: simulate what happens in the real prompt loop —
        // merge_tool_results puts the tool data onto the input's tool_calls,
        // then add_message is called with empty tool_results for the final
        // round.
        let input2 = crate::config::input::from_str(&global_config, "hello", Some(agent));
        let merged_input =
            input2.merge_tool_results("calling tool".to_string(), None, tool_results);
        assert!(
            merged_input.tool_calls().is_some(),
            "merged input should have tool_calls set"
        );
        super::add_message(&mut session, &merged_input, "final answer", None, &[]).unwrap();

        let tool_count_after_round2 = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .count();
        assert_eq!(
            tool_count_after_round2, 1,
            "round 2 should NOT add another Tool message — tool results were already saved in round 1; got {} Tool messages",
            tool_count_after_round2
        );

        // Verify on-disk content doesn't have duplicates either.
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        let tool_entry_count = content.matches("my_tool").count();
        // The tool name appears once in the tool results entry.
        assert!(
            tool_entry_count <= 2,
            "session file should not have excessive duplicates of tool data; found {tool_entry_count} occurrences of 'my_tool'"
        );
    }

    /// Verify that session file round-trips correctly after incremental
    /// saving: load the saved file and check messages match.
    #[test]
    fn incremental_session_round_trips_through_load() {
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config.clone()));

        let tool_results = vec![ToolResult::new(
            ToolCall {
                name: "search".to_string(),
                arguments: json!({"query": "test"}),
                id: Some("c1".to_string()),
                thought_signature: None,
            },
            json!({"results": ["a", "b"]}),
        )];

        // Round 1: intermediate save with tool results
        let input1 =
            crate::config::input::from_str(&global_config, "find test", Some(agent.clone()));
        super::add_message(&mut session, &input1, "searching...", None, &tool_results).unwrap();

        // Round 2: final answer
        let input2 = crate::config::input::from_str(&global_config, "find test", Some(agent));
        super::add_message(&mut session, &input2, "found results", None, &[]).unwrap();

        // Verify the saved session file contains all expected content
        // in correct order (header, messages from both rounds).
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();

        // Must have the header
        assert!(
            content.starts_with("type: header"),
            "file should start with header"
        );

        // Must contain the user input, both assistant outputs, and tool data
        assert!(
            content.contains("find test"),
            "file should contain user input"
        );
        assert!(
            content.contains("searching..."),
            "file should contain round 1 assistant output"
        );
        assert!(
            content.contains("found results"),
            "file should contain round 2 assistant output"
        );
        assert!(
            content.contains("search"),
            "file should contain tool call name"
        );

        // Count the YAML document separators to ensure the right number
        // of entries were written (header + N messages).
        let doc_count = content.matches("\n---\n").count() + 1; // +1 for first doc
        assert!(
            doc_count >= 4,
            "file should have at least 4 YAML documents (header + user + assistant + tool + assistant); got {doc_count}"
        );

        // Exercise the deserializer to catch parser/serde regressions.
        // We parse the YAML log entries directly (same path `Session::load`
        // uses internally via `SessionLogEntry::deserialize`) rather than
        // calling `Session::load`, because that also performs model
        // resolution which depends on the global model catalog and is not
        // available in this test's minimal `Config::default`.
        use serde::Deserialize;
        let mut parsed_messages: Vec<Message> = Vec::new();
        for document in serde_yaml::Deserializer::from_str(&content) {
            let entry =
                SessionLogEntry::deserialize(document).expect("log entry should round-trip");
            if let SessionLogEntry::Message { role, content } = entry {
                parsed_messages.push(Message::new(role, content));
            }
        }
        assert_eq!(
            parsed_messages.len(),
            session.messages.len(),
            "reloaded messages should match the original count"
        );
        assert_eq!(
            parsed_messages.last().unwrap().content.to_text(),
            "found results",
            "final reloaded message should preserve the last assistant output"
        );
    }

    /// Simulates the ACP server flow: save_message with &[] for the
    /// assistant output, then append_tool_round separately, then
    /// save_message again for the next round.  The session should have
    /// no duplicate messages and continuation detection should work.
    #[test]
    fn append_tool_round_enables_continuation_detection() {
        use crate::client::MessageContentToolCalls;
        use crate::tool::{ToolCall, ToolResult};
        use serde_json::json;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let mut session = test_session();
        session.set_sessions_dir(tmp.path().to_path_buf());

        let config = Config::default();
        let agent = config.extract_agent();
        let global_config = std::sync::Arc::new(parking_lot::RwLock::new(config));

        // Round 1: save assistant output (as ACP server does with &[])
        let input1 = crate::config::input::from_str(&global_config, "hello", Some(agent.clone()));
        super::add_message(&mut session, &input1, "calling tool", None, &[]).unwrap();
        assert_eq!(
            session.messages.last().unwrap().role,
            MessageRole::Assistant,
            "after add_message with no tool_results, last msg should be Assistant"
        );

        // ACP server then appends tool results via append_tool_round
        let tool_results = vec![ToolResult::new(
            ToolCall {
                name: "acp_tool".to_string(),
                arguments: json!({"q": "test"}),
                id: Some("tc1".to_string()),
                thought_signature: None,
            },
            json!("tool output"),
        )];
        let tool_msg = Message::new(
            MessageRole::Tool,
            crate::client::MessageContent::ToolCalls(MessageContentToolCalls::new(
                tool_results,
                "calling tool".to_string(),
                None,
            )),
        );
        super::append_tool_round(&mut session, &tool_msg);
        assert_eq!(
            session.messages.last().unwrap().role,
            MessageRole::Tool,
            "after append_tool_round, last msg should be Tool"
        );

        // Round 2: save final answer — should detect continuation and
        // NOT add a duplicate user message.
        let input2 = crate::config::input::from_str(&global_config, "hello", Some(agent));
        super::add_message(&mut session, &input2, "final answer", None, &[]).unwrap();

        let user_count = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count();
        assert_eq!(
            user_count, 1,
            "should have exactly 1 user message (from round 1), not duplicates from continuation"
        );

        let tool_count = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .count();
        assert_eq!(
            tool_count, 1,
            "should have exactly 1 tool message (from append_tool_round)"
        );

        // Verify the file contains the expected content
        let content = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        assert!(
            content.contains("acp_tool"),
            "file should contain the tool call name"
        );
        assert!(
            content.contains("final answer"),
            "file should contain the final assistant output"
        );
    }

    #[test]
    fn render_shows_model_fallbacks() {
        use crate::render::{MarkdownRender, RenderOptions};

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
