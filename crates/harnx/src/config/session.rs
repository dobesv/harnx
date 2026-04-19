use super::input::*;
use super::*;

use crate::client::{
    render_message_input, CompletionTokenUsage, Message, MessageContent, MessageRole,
};
use crate::render::MarkdownRender;

use anyhow::{bail, Context, Result};
use fancy_regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fs::{read_to_string, write, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::LazyLock;

static RE_AUTONAME_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{8}T\d{6}-").unwrap());

/// A single entry in the append-only session log file.
///
/// Session files use multi-document YAML (separated by `---`).
/// The first document is always a `Header`; subsequent documents are events.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum SessionLogEntry {
    #[serde(rename = "header")]
    Header {
        #[serde(rename = "model")]
        model_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        temperature: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        top_p: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        use_tools: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        save_session: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        compress_threshold: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        agent_variables: AgentVariables,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        agent_instructions: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        model_fallbacks: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compaction_agent: Option<String>,
    },
    #[serde(rename = "message")]
    Message {
        role: MessageRole,
        content: MessageContent,
    },
    #[serde(rename = "data_urls")]
    DataUrls { urls: HashMap<String, String> },
    #[serde(rename = "compress")]
    Compress { prompt: String },
    #[serde(rename = "clear")]
    Clear,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Session {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::deserialize_use_tools"
    )]
    use_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    save_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compress_threshold: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    agent_variables: AgentVariables,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    agent_instructions: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    model_fallbacks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compaction_agent: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    compressed_messages: Vec<Message>,
    messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    data_urls: HashMap<String, String>,

    #[serde(skip)]
    model: Model,
    #[serde(skip)]
    agent_prompt: String,
    #[serde(skip)]
    name: String,
    #[serde(skip)]
    path: Option<String>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    save_session_this_time: bool,
    #[serde(skip)]
    compressing: bool,
    #[serde(skip)]
    autoname: Option<AutoName>,
    #[serde(skip)]
    sessions_dir: Option<PathBuf>,
    #[serde(skip)]
    resolved_save_name: Option<(PathBuf, String)>,
    #[serde(skip)]
    tokens: usize,
    #[serde(skip)]
    completion_usage: CompletionTokenUsage,
}

impl Session {
    pub fn new(config: &Config, name: &str) -> Self {
        let agent = config.extract_agent();
        let mut session = Self {
            name: name.to_string(),
            save_session: config.save_session,
            ..Default::default()
        };
        session.set_agent(&agent);
        session.dirty = false;
        session
    }

    pub fn load(config: &Config, name: &str, path: &Path) -> Result<Self> {
        let content = read_to_string(path)
            .with_context(|| format!("Failed to load session {} at {}", name, path.display()))?;

        // Detect format: new log format has "type: header" as the first
        // meaningful line. Old format files are silently treated as empty
        // sessions (no crash, but content is not loaded).
        let session = if Self::is_log_format(&content) {
            Self::load_from_log(config, name, path, &content)?
        } else {
            // Old format: create a fresh session so we don't crash.
            let mut session = Self::new(config, name);
            Self::apply_name_and_path(&mut session, name, path, config);
            session
        };

        Ok(session)
    }

    fn is_log_format(content: &str) -> bool {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed == "---" {
                continue;
            }
            return trimmed == "type: header";
        }
        false
    }

    fn load_from_log(config: &Config, name: &str, path: &Path, content: &str) -> Result<Self> {
        let mut session = Self::default();

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

        session.model = crate::client::retrieve_model(config, &session.model_id, ModelType::Chat)?;
        Self::apply_name_and_path(&mut session, name, path, config);
        session.update_tokens();
        Ok(session)
    }

    fn apply_name_and_path(session: &mut Self, name: &str, path: &Path, config: &Config) {
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

    pub fn set_sessions_dir(&mut self, dir: PathBuf) {
        self.sessions_dir = Some(dir);
    }

    /// Initialize the session log file with a header entry.
    /// Called lazily on the first append_event when a path hasn't been
    /// established yet.  Best-effort: filesystem errors are silently
    /// ignored so the session can still be used in-memory.
    fn ensure_log_file(&mut self) {
        if self.save_session == Some(false) {
            return;
        }
        if self.path.is_some() {
            return;
        }
        let Some(sessions_dir) = self.sessions_dir.clone() else {
            return;
        };

        let (dir, session_name) = self.resolve_save_path(&sessions_dir);
        let session_path = dir.join(format!("{session_name}.yaml"));
        if ensure_parent_exists(&session_path).is_err() {
            return;
        }

        let header = self.build_header_entry();
        let Ok(content) = serde_yaml::to_string(&header) else {
            return;
        };
        if write(&session_path, &content).is_ok() {
            self.path = Some(session_path.display().to_string());
        }
    }

    /// Append a log entry to the session file.
    /// Lazily initializes the log file on the first call.
    /// Returns true if the entry was successfully written.
    fn append_event(&mut self, entry: &SessionLogEntry) -> bool {
        self.ensure_log_file();
        let Some(path_str) = &self.path else {
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

    fn build_header_entry(&self) -> SessionLogEntry {
        SessionLogEntry::Header {
            model_id: self.model_id.clone(),
            temperature: self.temperature,
            top_p: self.top_p,
            use_tools: self.use_tools.clone(),
            save_session: self.save_session,
            compress_threshold: self.compress_threshold,
            agent_name: self.agent_name.clone(),
            agent_variables: self.agent_variables.clone(),
            agent_instructions: self.agent_instructions.clone(),
            model_fallbacks: self.model_fallbacks.clone(),
            compaction_agent: self.compaction_agent.clone(),
        }
    }

    fn resolve_save_path(&mut self, session_dir: &Path) -> (PathBuf, String) {
        if let Some((dir, name)) = self.resolved_save_name.clone() {
            // Update the cached name with autoname if it arrived since
            // the first resolution.
            if self.name == TEMP_SESSION_NAME && !name.contains('-') {
                if let Some(autoname) = self.autoname() {
                    let name = format!("{name}-{autoname}");
                    self.resolved_save_name = Some((dir.clone(), name.clone()));
                    return (dir, name);
                }
            }
            return (dir, name);
        }
        let mut dir = session_dir.to_path_buf();
        let mut name = self.name.clone();
        if name == TEMP_SESSION_NAME {
            dir = dir.join("_");
            let now = chrono::Local::now();
            name = now.format("%Y%m%dT%H%M%S").to_string();
            if let Some(autoname) = self.autoname() {
                name = format!("{name}-{autoname}");
            }
        }
        self.resolved_save_name = Some((dir.clone(), name.clone()));
        (dir, name)
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.compressed_messages.is_empty()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn agent_name(&self) -> Option<&str> {
        self.agent_name.as_deref()
    }

    pub fn save_session(&self) -> Option<bool> {
        self.save_session
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    pub fn update_tokens(&mut self) {
        self.tokens = self.model().total_tokens(&self.messages);
    }

    pub fn completion_usage(&self) -> &CompletionTokenUsage {
        &self.completion_usage
    }

    pub fn add_completion_usage(&mut self, usage: &CompletionTokenUsage) {
        self.completion_usage.accumulate(usage);
    }

    pub fn has_user_messages(&self) -> bool {
        self.messages.iter().any(|v| v.role.is_user())
    }

    pub fn export(&self) -> Result<String> {
        let mut data = json!({
            "path": self.path,
            "model": self.model().id(),
        });
        if let Some(temperature) = self.temperature() {
            data["temperature"] = temperature.into();
        }
        if let Some(top_p) = self.top_p() {
            data["top_p"] = top_p.into();
        }
        if let Some(use_tools) = self.use_tools() {
            data["use_tools"] = serde_json::Value::Array(
                use_tools
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            );
        }
        if !self.model_fallbacks.is_empty() {
            data["model_fallbacks"] = serde_json::Value::Array(
                self.model_fallbacks
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            );
        }
        if let Some(save_session) = self.save_session() {
            data["save_session"] = save_session.into();
        }
        let (tokens, percent) = self.tokens_usage();
        data["total_tokens"] = tokens.into();
        if let Some(max_input_tokens) = self.model().max_input_tokens() {
            data["max_input_tokens"] = max_input_tokens.into();
        }
        if percent != 0.0 {
            data["total/max"] = format!("{percent}%").into();
        }
        data["messages"] = json!(self.messages);

        let output = serde_yaml::to_string(&data)
            .with_context(|| format!("Unable to show info about session '{}'", &self.name))?;
        Ok(output)
    }

    pub fn render(
        &self,
        render: &mut MarkdownRender,
        agent_info: &Option<(String, Vec<String>)>,
    ) -> Result<String> {
        let mut items = vec![];

        if let Some(path) = &self.path {
            items.push(("path", path.to_string()));
        }

        if let Some(autoname) = self.autoname() {
            items.push(("autoname", autoname.to_string()));
        }

        items.push(("model", self.model().id()));

        if let Some(temperature) = self.temperature() {
            items.push(("temperature", temperature.to_string()));
        }
        if let Some(top_p) = self.top_p() {
            items.push(("top_p", top_p.to_string()));
        }

        if let Some(use_tools) = self.use_tools() {
            items.push(("use_tools", use_tools.join(",")));
        }

        if !self.model_fallbacks.is_empty() {
            items.push(("model_fallbacks", self.model_fallbacks.join(",")));
        }

        if let Some(save_session) = self.save_session() {
            items.push(("save_session", save_session.to_string()));
        }

        if let Some(compress_threshold) = self.compress_threshold {
            items.push(("compress_threshold", compress_threshold.to_string()));
        }

        if let Some(max_input_tokens) = self.model().max_input_tokens() {
            items.push(("max_input_tokens", max_input_tokens.to_string()));
        }

        let mut lines: Vec<String> = items
            .iter()
            .map(|(name, value)| format!("{name:<20}{value}"))
            .collect();

        lines.push(String::new());

        if !self.is_empty() {
            let resolve_url_fn = |url: &str| resolve_data_url(&self.data_urls, url.to_string());

            for message in &self.messages {
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

    pub fn tokens_usage(&self) -> (usize, f32) {
        let tokens = self.tokens();
        let max_input_tokens = self.model().max_input_tokens().unwrap_or_default();
        let percent = if max_input_tokens == 0 {
            0.0
        } else {
            let percent = tokens as f32 / max_input_tokens as f32 * 100.0;
            (percent * 100.0).round() / 100.0
        };
        (tokens, percent)
    }

    pub fn set_agent(&mut self, agent: &Agent) {
        self.model_id = agent.model().id();
        self.temperature = agent.temperature();
        self.top_p = agent.top_p();
        self.use_tools = agent.use_tools();
        self.model_fallbacks = agent.model_fallbacks().to_vec();
        self.compaction_agent = agent.compaction_agent().map(str::to_string);
        self.model = agent.model().clone();
        self.agent_name = convert_option_string(agent.name());
        self.agent_prompt = agent.interpolated_instructions();
        self.agent_variables = agent.variables().clone();
        self.agent_instructions = self.agent_prompt.clone();
        self.dirty = true;
        self.update_tokens();
    }

    pub fn sync_agent(&mut self, agent: &Agent) {
        self.agent_name = convert_option_string(agent.name());
        self.agent_prompt = agent.interpolated_instructions();
        self.agent_variables = agent.variables().clone();
        self.agent_instructions = self.agent_prompt.clone();
    }

    pub fn agent_variables(&self) -> &AgentVariables {
        &self.agent_variables
    }

    pub fn set_save_session(&mut self, value: Option<bool>) {
        if self.save_session != value {
            self.save_session = value;
            self.dirty = true;
        }
    }

    pub fn set_save_session_this_time(&mut self) {
        self.save_session_this_time = true;
    }

    /// Test-only helper: directly inject a message into the session without
    /// going through the full save/log machinery.  Used to set up compaction
    /// test scenarios.
    #[cfg(test)]
    pub(crate) fn push_message_for_test(&mut self, role: crate::client::MessageRole, text: String) {
        self.messages.push(crate::client::Message::new(
            role,
            crate::client::MessageContent::Text(text),
        ));
    }

    /// Append a pre-built Tool message to the session log.
    /// Used by the ACP server to persist tool results separately from
    /// the main `add_message` flow.
    pub fn append_tool_round(&mut self, tool_msg: &Message) {
        if !self.append_event(&SessionLogEntry::Message {
            role: tool_msg.role,
            content: tool_msg.content.clone(),
        }) {
            self.dirty = true;
        }
        self.messages.push(tool_msg.clone());
        self.update_tokens();
    }

    pub fn set_compress_threshold(&mut self, value: Option<usize>) {
        if self.compress_threshold != value {
            self.compress_threshold = value;
            self.dirty = true;
        }
    }

    pub fn need_compress(&self, global_compress_threshold: usize) -> bool {
        if self.compressing {
            return false;
        }
        let threshold = self.compress_threshold.unwrap_or(global_compress_threshold);
        if threshold < 1 {
            return false;
        }
        self.tokens() > threshold
    }

    pub fn compressing(&self) -> bool {
        self.compressing
    }

    pub fn set_compressing(&mut self, compressing: bool) {
        self.compressing = compressing;
    }

    pub fn compress(&mut self, mut prompt: String) {
        if let Some(system_prompt) = self.messages.first().and_then(|v| {
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
        self.compressed_messages.append(&mut self.messages);
        self.messages.push(Message::new(
            MessageRole::System,
            MessageContent::Text(prompt.clone()),
        ));
        self.update_tokens();
        if !self.append_event(&SessionLogEntry::Compress { prompt }) {
            self.dirty = true;
        }
    }

    pub fn need_autoname(&self) -> bool {
        self.autoname.as_ref().map(|v| v.need()).unwrap_or_default()
    }

    pub fn set_autonaming(&mut self, naming: bool) {
        if let Some(v) = self.autoname.as_mut() {
            v.naming = naming;
        }
    }

    pub fn chat_history_for_autonaming(&self) -> Option<String> {
        self.autoname.as_ref().and_then(|v| v.chat_history.clone())
    }

    pub fn autoname(&self) -> Option<&str> {
        self.autoname.as_ref().and_then(|v| v.name.as_deref())
    }

    pub fn set_autoname(&mut self, value: &str) {
        let name = value
            .chars()
            .map(|v| if v.is_alphanumeric() { v } else { '-' })
            .collect();
        self.autoname = Some(AutoName::new(name));
    }

    pub fn exit(&mut self, session_dir: &Path, is_tui: bool) -> Result<()> {
        if self.save_session == Some(false) && !self.save_session_this_time {
            return Ok(());
        }
        if !self.dirty {
            // Nothing new to persist, but print the path if the log file exists.
            if is_tui {
                if let Some(path) = &self.path {
                    println!("✓ Session saved at '{path}'.");
                }
            }
            return Ok(());
        }
        // Session has unsaved changes that were not yet appended (e.g. legacy
        // callers or sessions that didn't go through init_log). Do a full save.
        let (session_dir, session_name) = self.resolve_save_path(session_dir);
        let session_path = session_dir.join(format!("{session_name}.yaml"));
        self.save(&session_name, &session_path, is_tui)?;
        Ok(())
    }

    /// Full save: rewrites the entire session file in log format.
    /// Used as a fallback when events were not incrementally appended.
    pub fn save(&mut self, session_name: &str, session_path: &Path, is_tui: bool) -> Result<()> {
        ensure_parent_exists(session_path)?;

        self.path = Some(session_path.display().to_string());

        // Write in the new log format.
        let mut content = serde_yaml::to_string(&self.build_header_entry())
            .with_context(|| format!("Failed to serialize session header for '{}'", self.name))?;
        for msg in &self.compressed_messages {
            let entry = SessionLogEntry::Message {
                role: msg.role,
                content: msg.content.clone(),
            };
            content.push_str("---\n");
            content.push_str(
                &serde_yaml::to_string(&entry)
                    .with_context(|| format!("Failed to serialize message in '{}'", self.name))?,
            );
        }
        if !self.compressed_messages.is_empty() {
            // Write a compress entry to mark the boundary.
            // Only write it and skip the first message if the first message
            // is actually a system message from compression.
            let wrote_compress = if let Some(system_msg) = self.messages.first() {
                if system_msg.role == MessageRole::System {
                    let compress_entry = SessionLogEntry::Compress {
                        prompt: system_msg.content.to_text(),
                    };
                    content.push_str("---\n");
                    content.push_str(&serde_yaml::to_string(&compress_entry).with_context(
                        || format!("Failed to serialize compress entry in '{}'", self.name),
                    )?);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            // Write remaining messages (skip the system message from compress only if we wrote a compress entry).
            let start_idx = if wrote_compress { 1 } else { 0 };
            for msg in self.messages.iter().skip(start_idx) {
                let entry = SessionLogEntry::Message {
                    role: msg.role,
                    content: msg.content.clone(),
                };
                content.push_str("---\n");
                content.push_str(
                    &serde_yaml::to_string(&entry).with_context(|| {
                        format!("Failed to serialize message in '{}'", self.name)
                    })?,
                );
            }
        } else {
            for msg in &self.messages {
                let entry = SessionLogEntry::Message {
                    role: msg.role,
                    content: msg.content.clone(),
                };
                content.push_str("---\n");
                content.push_str(
                    &serde_yaml::to_string(&entry).with_context(|| {
                        format!("Failed to serialize message in '{}'", self.name)
                    })?,
                );
            }
        }
        if !self.data_urls.is_empty() {
            let entry = SessionLogEntry::DataUrls {
                urls: self.data_urls.clone(),
            };
            content.push_str("---\n");
            content
                .push_str(&serde_yaml::to_string(&entry).with_context(|| {
                    format!("Failed to serialize data_urls in '{}'", self.name)
                })?);
        }

        write(session_path, content).with_context(|| {
            format!(
                "Failed to write session '{}' to '{}'",
                self.name,
                session_path.display()
            )
        })?;

        if is_tui {
            println!("✓ Saved the session to '{}'.", session_path.display());
        }

        if self.name() != session_name {
            self.name = session_name.to_string()
        }

        self.dirty = false;

        Ok(())
    }

    pub fn guard_empty(&self) -> Result<()> {
        if !self.is_empty() {
            bail!("Cannot perform this operation because the session has messages, please `.empty session` first.");
        }
        Ok(())
    }

    pub fn add_message(
        &mut self,
        input: &Input,
        output: &str,
        thought: Option<&str>,
        tool_results: &[crate::tool::ToolResult],
    ) -> Result<()> {
        if input.continue_output().is_some() {
            if let Some(message) = self.messages.last_mut() {
                if let MessageContent::Text(text) = &mut message.content {
                    *text = format!("{text}{output}");
                }
            }
            // Continue/regenerate are edits to the last message; mark dirty
            // so the full-save fallback can persist them. We don't append
            // because they modify an existing entry.
            self.dirty = true;
        } else if input.regenerate() {
            if let Some(message) = self.messages.last_mut() {
                if let MessageContent::Text(text) = &mut message.content {
                    *text = output.to_string();
                }
            }
            self.dirty = true;
        } else {
            let mut all_appended = true;
            // Detect continuation rounds: if the last saved message is a Tool
            // message, we're continuing after tool execution and should NOT add
            // a duplicate user message.
            let is_continuation = self
                .messages
                .last()
                .is_some_and(|m| m.role == MessageRole::Tool);
            if self.messages.is_empty() {
                if self.name == TEMP_SESSION_NAME && self.save_session != Some(false) {
                    let raw_input = input.raw();
                    let chat_history = format!("USER: {raw_input}\nASSISTANT: {output}\n");
                    self.autoname = Some(AutoName::new_from_chat_history(chat_history));
                }
                let agent_messages = input.agent().build_messages(input);
                for msg in &agent_messages {
                    all_appended &= self.append_event(&SessionLogEntry::Message {
                        role: msg.role,
                        content: msg.content.clone(),
                    });
                }
                self.messages.extend(agent_messages);
            } else if !is_continuation {
                let user_msg = Message::new(MessageRole::User, input.message_content());
                all_appended &= self.append_event(&SessionLogEntry::Message {
                    role: user_msg.role,
                    content: user_msg.content.clone(),
                });
                self.messages.push(user_msg);
            }
            let new_data_urls = input.data_urls();
            if !new_data_urls.is_empty() {
                all_appended &= self.append_event(&SessionLogEntry::DataUrls {
                    urls: new_data_urls.clone(),
                });
            }
            self.data_urls.extend(new_data_urls);
            // Only process input.tool_calls() when this is NOT a
            // continuation round.  On continuation rounds the tool results
            // were already persisted incrementally via the `tool_results`
            // parameter in the previous round, so replaying them from the
            // merged input would create duplicates.
            if !is_continuation {
                if let Some(tool_calls) = input.tool_calls().clone() {
                    let tool_msg =
                        Message::new(MessageRole::Tool, MessageContent::ToolCalls(tool_calls));
                    all_appended &= self.append_event(&SessionLogEntry::Message {
                        role: tool_msg.role,
                        content: tool_msg.content.clone(),
                    });
                    self.messages.push(tool_msg);
                }
            }
            if let Some(injected) = input.injected_user_text() {
                let injected_msg = Message::new(
                    MessageRole::User,
                    MessageContent::Text(injected.to_string()),
                );
                all_appended &= self.append_event(&SessionLogEntry::Message {
                    role: injected_msg.role,
                    content: injected_msg.content.clone(),
                });
                self.messages.push(injected_msg);
            }
            let content = match thought {
                Some(v) => MessageContent::Text(format!("<think>\n{v}\n</think>\n{output}")),
                _ => MessageContent::Text(output.to_string()),
            };
            let assistant_msg = Message::new(MessageRole::Assistant, content);
            all_appended &= self.append_event(&SessionLogEntry::Message {
                role: assistant_msg.role,
                content: assistant_msg.content.clone(),
            });
            self.messages.push(assistant_msg);
            // Append tool results from this round (incremental persistence).
            if !tool_results.is_empty() {
                let tool_calls_content =
                    MessageContent::ToolCalls(crate::client::MessageContentToolCalls::new(
                        tool_results.to_vec(),
                        output.to_string(),
                        thought.map(str::to_string),
                    ));
                let tool_msg = Message::new(MessageRole::Tool, tool_calls_content);
                all_appended &= self.append_event(&SessionLogEntry::Message {
                    role: tool_msg.role,
                    content: tool_msg.content.clone(),
                });
                self.messages.push(tool_msg);
            }
            // Only clear dirty if all events were appended; otherwise the
            // full-save fallback in exit() will persist the data.
            self.dirty = !all_appended;
        }
        self.update_tokens();
        Ok(())
    }

    pub fn clear_messages(&mut self) {
        self.messages.clear();
        self.compressed_messages.clear();
        self.data_urls.clear();
        self.autoname = None;
        self.completion_usage = CompletionTokenUsage::default();
        self.update_tokens();
        if !self.append_event(&SessionLogEntry::Clear) {
            self.dirty = true;
        }
    }

    pub fn echo_messages(&self, input: &Input) -> String {
        let messages = self.build_messages(input);
        serde_yaml::to_string(&messages).unwrap_or_else(|_| "Unable to echo message".into())
    }

    pub fn build_messages(&self, input: &Input) -> Vec<Message> {
        let mut messages = self.messages.clone();
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
        } else if len == 1 && self.compressed_messages.len() >= 2 {
            if let Some(index) = self
                .compressed_messages
                .iter()
                .rposition(|v| v.role == MessageRole::User)
            {
                messages.extend(self.compressed_messages[index..].to_vec());
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
}

impl Session {
    pub fn to_agent(&self) -> Agent {
        let agent_name = self.agent_name.as_deref().unwrap_or(TEMP_AGENT_NAME);
        let prompt = if self.agent_prompt.is_empty() {
            self.agent_instructions.as_str()
        } else {
            self.agent_prompt.as_str()
        };
        let mut agent = Agent::from_markdown(agent_name, prompt);
        agent.set_model(self.model.clone());
        agent.set_temperature(self.temperature);
        agent.set_top_p(self.top_p);
        agent.set_use_tools(self.use_tools.clone());
        agent.set_model_fallbacks(self.model_fallbacks.clone());
        agent.set_compaction_agent(self.compaction_agent.clone());
        agent.set_shared_variables(self.agent_variables.clone());
        agent
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    pub fn top_p(&self) -> Option<f64> {
        self.top_p
    }

    pub fn use_tools(&self) -> Option<Vec<String>> {
        self.use_tools.clone()
    }

    pub fn set_model(&mut self, model: Model) {
        if self.model().id() != model.id() {
            self.model_id = model.id();
            self.model = model;
            self.dirty = true;
            self.update_tokens();
        }
    }

    pub fn set_temperature(&mut self, value: Option<f64>) {
        if self.temperature != value {
            self.temperature = value;
            self.dirty = true;
        }
    }

    pub fn set_top_p(&mut self, value: Option<f64>) {
        if self.top_p != value {
            self.top_p = value;
            self.dirty = true;
        }
    }

    pub fn set_use_tools(&mut self, value: Option<Vec<String>>) {
        if self.use_tools != value {
            self.use_tools = value;
            self.dirty = true;
        }
    }

    #[cfg(test)]
    pub fn model_fallbacks(&self) -> &[String] {
        &self.model_fallbacks
    }

    pub fn set_model_fallbacks(&mut self, value: Vec<String>) {
        if self.model_fallbacks != value {
            self.model_fallbacks = value;
            self.dirty = true;
        }
    }

    pub fn set_compaction_agent(&mut self, value: Option<String>) {
        if self.compaction_agent != value {
            self.compaction_agent = value;
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> Session {
        Session::new(&Config::default(), "test")
    }

    #[test]
    fn set_agent_to_agent_round_trip_preserves_model_fallbacks() {
        let agent = Agent::from_markdown(
            "test",
            "---\nmodel: openai:gpt-4o\nmodel_fallbacks:\n  - anthropic:claude\n  - google:gemini\n---\nYou are a test agent.",
        );
        let mut session = test_session();

        session.set_agent(&agent);
        let round_tripped_agent = session.to_agent();

        assert_eq!(
            round_tripped_agent.model_fallbacks(),
            agent.model_fallbacks()
        );
    }

    #[test]
    fn session_header_serde_round_trip_preserves_model_fallbacks() {
        let mut session = test_session();
        session.set_model_fallbacks(vec![
            "anthropic:claude".to_string(),
            "google:gemini".to_string(),
        ]);

        let yaml = serde_yaml::to_string(&session.build_header_entry()).unwrap();
        let entry: SessionLogEntry = serde_yaml::from_str(&yaml).unwrap();

        match entry {
            SessionLogEntry::Header {
                model_fallbacks, ..
            } => {
                assert_eq!(
                    model_fallbacks,
                    vec!["anthropic:claude".to_string(), "google:gemini".to_string()]
                );
            }
            _ => panic!("expected header entry"),
        }
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
    fn set_model_fallbacks_updates_session_and_marks_dirty() {
        let mut session = test_session();

        assert!(session.model_fallbacks().is_empty());

        session.set_model_fallbacks(vec!["anthropic:claude".to_string()]);

        assert_eq!(session.model_fallbacks(), &["anthropic:claude".to_string()]);
        assert!(session.dirty);
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
        let input = Input::from_str(
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
        session
            .add_message(&input, "I'll call a tool", None, &tool_results)
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
        let input2 = Input::from_str(
            &std::sync::Arc::new(parking_lot::RwLock::new(config)),
            "hello",
            Some(agent),
        );
        session
            .add_message(&input2, "Here is the result", None, &[])
            .unwrap();

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
        let input1 = Input::from_str(&global_config, "hello", Some(agent.clone()));
        session
            .add_message(&input1, "calling tool", None, &tool_results)
            .unwrap();

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
        let input2 = Input::from_str(&global_config, "hello", Some(agent));
        let merged_input =
            input2.merge_tool_results("calling tool".to_string(), None, tool_results);
        assert!(
            merged_input.tool_calls().is_some(),
            "merged input should have tool_calls set"
        );
        session
            .add_message(&merged_input, "final answer", None, &[])
            .unwrap();

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
        let input1 = Input::from_str(&global_config, "find test", Some(agent.clone()));
        session
            .add_message(&input1, "searching...", None, &tool_results)
            .unwrap();

        // Round 2: final answer
        let input2 = Input::from_str(&global_config, "find test", Some(agent));
        session
            .add_message(&input2, "found results", None, &[])
            .unwrap();

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
        let input1 = Input::from_str(&global_config, "hello", Some(agent.clone()));
        session
            .add_message(&input1, "calling tool", None, &[])
            .unwrap();
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
        session.append_tool_round(&tool_msg);
        assert_eq!(
            session.messages.last().unwrap().role,
            MessageRole::Tool,
            "after append_tool_round, last msg should be Tool"
        );

        // Round 2: save final answer — should detect continuation and
        // NOT add a duplicate user message.
        let input2 = Input::from_str(&global_config, "hello", Some(agent));
        session
            .add_message(&input2, "final answer", None, &[])
            .unwrap();

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
        let mut render = MarkdownRender::init(options).unwrap();
        let agent_info: Option<(String, Vec<String>)> = None;
        let output = session.render(&mut render, &agent_info).unwrap();

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

#[derive(Debug, Clone, Default)]
struct AutoName {
    naming: bool,
    chat_history: Option<String>,
    name: Option<String>,
}

impl AutoName {
    pub fn new(name: String) -> Self {
        Self {
            name: Some(name),
            ..Default::default()
        }
    }
    pub fn new_from_chat_history(chat_history: String) -> Self {
        Self {
            chat_history: Some(chat_history),
            ..Default::default()
        }
    }
    pub fn need(&self) -> bool {
        !self.naming && self.chat_history.is_some() && self.name.is_none()
    }
}
