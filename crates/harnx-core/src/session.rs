//! `Session` — persistent conversation history + lifecycle metadata.
//! Pure data + pure methods. File I/O (save, exit, ensure_log_file,
//! append_event), Config-using operations (new, load, render), and
//! methods with harnx-only dependencies (add_message, compress,
//! build_messages, echo_messages, etc.) live in
//! `harnx::config::session` as free functions.

use crate::agent_config::{AgentConfig, AgentVariables, TEMP_AGENT_NAME};
use crate::api_types::CompletionTokenUsage;
use crate::message::{Message, MessageContent, MessageRole};
use crate::model::Model;

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

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
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::agent_config::deserialize_use_tools"
    )]
    pub use_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compress_threshold: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub agent_variables: AgentVariables,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_instructions: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_fallbacks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_agent: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compressed_messages: Vec<Message>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub data_urls: HashMap<String, String>,

    #[serde(skip)]
    pub model: Model,
    #[serde(skip)]
    pub agent_prompt: String,
    #[serde(skip)]
    pub name: String,
    #[serde(skip)]
    pub path: Option<String>,
    #[serde(skip)]
    pub dirty: bool,
    #[serde(skip)]
    pub save_session_this_time: bool,
    #[serde(skip)]
    pub compressing: bool,
    #[serde(skip)]
    pub autoname: Option<AutoName>,
    #[serde(skip)]
    pub sessions_dir: Option<PathBuf>,
    #[serde(skip)]
    pub resolved_save_name: Option<(PathBuf, String)>,
    #[serde(skip)]
    pub tokens: usize,
    #[serde(skip)]
    pub completion_usage: CompletionTokenUsage,
}

impl Session {
    pub fn set_sessions_dir(&mut self, dir: PathBuf) {
        self.sessions_dir = Some(dir);
    }

    pub fn is_log_format(content: &str) -> bool {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed == "---" {
                continue;
            }
            return trimmed == "type: header";
        }
        false
    }

    pub fn build_header_entry(&self) -> SessionLogEntry {
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

    pub fn set_agent(&mut self, agent: &AgentConfig) {
        self.model_id = agent.model().id();
        self.temperature = agent.temperature();
        self.top_p = agent.top_p();
        self.use_tools = agent.use_tools();
        self.model_fallbacks = agent.model_fallbacks().to_vec();
        self.compaction_agent = agent.compaction_agent().map(str::to_string);
        self.model = agent.model().clone();
        self.agent_name = if agent.name().is_empty() {
            None
        } else {
            Some(agent.name().to_string())
        };
        self.agent_prompt = agent.interpolated_instructions();
        self.agent_variables = agent.variables().clone();
        self.agent_instructions = self.agent_prompt.clone();
        self.dirty = true;
        self.update_tokens();
    }

    pub fn sync_agent(&mut self, agent: &AgentConfig) {
        self.agent_name = if agent.name().is_empty() {
            None
        } else {
            Some(agent.name().to_string())
        };
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
    pub fn push_message_for_test(&mut self, role: MessageRole, text: String) {
        self.messages
            .push(Message::new(role, MessageContent::Text(text)));
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

    pub fn guard_empty(&self) -> Result<()> {
        if !self.is_empty() {
            bail!("Cannot perform this operation because the session has messages, please `.empty session` first.");
        }
        Ok(())
    }
}

impl Session {
    pub fn to_agent_config(&self) -> AgentConfig {
        let agent_name = self.agent_name.as_deref().unwrap_or(TEMP_AGENT_NAME);
        let prompt = if self.agent_prompt.is_empty() {
            self.agent_instructions.as_str()
        } else {
            self.agent_prompt.as_str()
        };
        let mut config = AgentConfig::from_markdown(agent_name, prompt);
        config.set_model(self.model.clone());
        config.set_temperature(self.temperature);
        config.set_top_p(self.top_p);
        config.set_use_tools(self.use_tools.clone());
        config.set_model_fallbacks(self.model_fallbacks.clone());
        config.set_compaction_agent(self.compaction_agent.clone());
        config.set_shared_variables(self.agent_variables.clone());
        config
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

#[derive(Debug, Clone, Default)]
pub struct AutoName {
    pub(crate) naming: bool,
    pub(crate) chat_history: Option<String>,
    pub(crate) name: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_header_serde_round_trip_preserves_model_fallbacks() {
        let mut session = Session::default();
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
    fn set_model_fallbacks_updates_session_and_marks_dirty() {
        let mut session = Session::default();

        assert!(session.model_fallbacks().is_empty());

        session.set_model_fallbacks(vec!["anthropic:claude".to_string()]);

        assert_eq!(session.model_fallbacks(), &["anthropic:claude".to_string()]);
        assert!(session.dirty);
    }
}
