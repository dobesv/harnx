use super::input::*;
use super::*;

use crate::client::{CompletionTokenUsage, Message, MessageContent, MessageRole};
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
    },
    #[serde(rename = "message")]
    Message {
        role: MessageRole,
        content: MessageContent,
    },
    #[serde(rename = "data_urls")]
    DataUrls {
        urls: HashMap<String, String>,
    },
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

        session.model = Model::retrieve_model(config, &session.model_id, ModelType::Chat)?;
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

    pub fn dirty(&self) -> bool {
        self.dirty
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

    pub fn user_messages_len(&self) -> usize {
        self.messages.iter().filter(|v| v.role.is_user()).count()
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
                        lines.push(
                            render
                                .render(&message.content.render_input(resolve_url_fn, agent_info)),
                        );
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
                            message.content.render_input(resolve_url_fn, agent_info)
                        ));
                    }
                    MessageRole::Tool => {
                        lines.push(message.content.render_input(resolve_url_fn, agent_info));
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

    pub fn exit(&mut self, session_dir: &Path, is_repl: bool) -> Result<()> {
        if self.save_session == Some(false) && !self.save_session_this_time {
            return Ok(());
        }
        if !self.dirty {
            // Nothing new to persist, but print the path if the log file exists.
            if is_repl {
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
        self.save(&session_name, &session_path, is_repl)?;
        Ok(())
    }

    /// Full save: rewrites the entire session file in log format.
    /// Used as a fallback when events were not incrementally appended.
    pub fn save(&mut self, session_name: &str, session_path: &Path, is_repl: bool) -> Result<()> {
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
            if let Some(system_msg) = self.messages.first() {
                if system_msg.role == MessageRole::System {
                    let compress_entry = SessionLogEntry::Compress {
                        prompt: system_msg.content.to_text(),
                    };
                    content.push_str("---\n");
                    content.push_str(&serde_yaml::to_string(&compress_entry).with_context(
                        || format!("Failed to serialize compress entry in '{}'", self.name),
                    )?);
                }
            }
            // Write remaining messages (skip the system message from compress).
            for msg in self.messages.iter().skip(1) {
                let entry = SessionLogEntry::Message {
                    role: msg.role,
                    content: msg.content.clone(),
                };
                content.push_str("---\n");
                content.push_str(&serde_yaml::to_string(&entry).with_context(|| {
                    format!("Failed to serialize message in '{}'", self.name)
                })?);
            }
        } else {
            for msg in &self.messages {
                let entry = SessionLogEntry::Message {
                    role: msg.role,
                    content: msg.content.clone(),
                };
                content.push_str("---\n");
                content.push_str(&serde_yaml::to_string(&entry).with_context(|| {
                    format!("Failed to serialize message in '{}'", self.name)
                })?);
            }
        }
        if !self.data_urls.is_empty() {
            let entry = SessionLogEntry::DataUrls {
                urls: self.data_urls.clone(),
            };
            content.push_str("---\n");
            content.push_str(&serde_yaml::to_string(&entry).with_context(|| {
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

        if is_repl {
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
            } else {
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
            if let Some(tool_calls) = input.tool_calls().clone() {
                let tool_msg =
                    Message::new(MessageRole::Tool, MessageContent::ToolCalls(tool_calls));
                all_appended &= self.append_event(&SessionLogEntry::Message {
                    role: tool_msg.role,
                    content: tool_msg.content.clone(),
                });
                self.messages.push(tool_msg);
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
