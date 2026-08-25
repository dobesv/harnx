//! `Session` — persistent conversation history + lifecycle metadata.
//! Pure data + pure methods. NATS persistence, Config-using operations, and
//! methods with harnx-only dependencies (add_message, compress,
//! build_messages, echo_messages, etc.) live in
//! `harnx::config::session` as free functions.

use crate::agent_config::{AgentConfig, AgentVariables, TEMP_AGENT_NAME};
use crate::api_types::CompletionTokenUsage;
use crate::message::{Message, MessageContent, MessageRole};
use crate::model::Model;
use crate::tool::{SwitchAgentData, ToolCall};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

/// A single conversation event in the append-only session transcript.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum SessionLogEntry {
    #[serde(rename = "message")]
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: MessageRole,
        content: MessageContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fence_token: Option<u64>,
    },
    /// Assistant turn that issued tool calls. The text/thought are the
    /// LLM's prose preceding the calls. This entry is written
    /// immediately after the LLM returns, before any tool executes, so
    /// that the transcript shows what was requested even if the process
    /// is interrupted mid-execution. It MUST be followed by a matching
    /// `ToolResults` entry; an orphan trailing `ToolCalls` is repaired
    /// on load by synthesizing lost-response errors.
    #[serde(rename = "tool_calls")]
    ToolCalls {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought: Option<String>,
        calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fence_token: Option<u64>,
    },
    #[serde(rename = "cancel")]
    Cancel { fence_token: u64 },
    /// Results for the immediately preceding `ToolCalls` entry.
    #[serde(rename = "tool_results")]
    ToolResults {
        results: Vec<ToolOutput>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<DateTime<Utc>>,
    },
    #[serde(rename = "data_urls")]
    DataUrls { urls: HashMap<String, String> },
    #[serde(rename = "compress")]
    Compress { prompt: String },
    #[serde(rename = "clear")]
    Clear,
    #[serde(rename = "edit_entries")]
    EditEntries {
        /// Inclusive range of entry sequence numbers being replaced.
        from: usize,
        to: usize,
        /// Replacement YAML documents (raw strings, one per replaced entry).
        /// Empty vec = deletion.
        replacements: Vec<String>,
    },
    #[serde(rename = "rewind")]
    Rewind {
        /// All entries with seq > after_seq are excluded from context on replay.
        after_seq: usize,
    },
    /// A turn that ended in a worker-side failure instead of an assistant
    /// reply. Written by the worker so attached clients stop waiting and the
    /// transcript records why the turn produced nothing. Unlike `Message`, an
    /// `Error` is never replayed into the model's context.
    #[serde(rename = "error")]
    Error {
        message: String,
        fence_token: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<DateTime<Utc>>,
    },
    /// Durable boundary written only after the worker has finished the full
    /// model/tool/stop-hook loop. Live `Turn::Ended` events are advisory and
    /// may be lost, so clients use this entry as the authoritative
    /// successful completion signal.
    #[serde(rename = "turn_end")]
    TurnEnd {
        /// Highest physical user-message sequence consumed by this turn. A
        /// later queued user row may precede this marker physically without
        /// having been incorporated into the completed turn.
        through_seq: u64,
        fence_token: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<DateTime<Utc>>,
    },
    #[serde(other)]
    Unknown,
}

impl SessionLogEntry {
    /// Stamp the fence token on worker-originated entries that carry one
    /// (`Message`, `ToolCalls`). Other variants are left unchanged: `Cancel`,
    /// `Error`, and `TurnEnd` carry their fence at construction, while
    /// client-originated entries (`UserMessage`) are intentionally unfenced.
    pub fn set_fence_token(&mut self, fence: u64) {
        match self {
            SessionLogEntry::Message { fence_token, .. }
            | SessionLogEntry::ToolCalls { fence_token, .. } => {
                *fence_token = Some(fence);
            }
            _ => {}
        }
    }

    /// Fence token carried by this entry, if any. `Message`/`ToolCalls` carry an
    /// optional fence; `Cancel`, `Error`, and `TurnEnd` always carry one. All
    /// other variants are unfenced and return `None`.
    pub fn fence_token(&self) -> Option<u64> {
        match self {
            SessionLogEntry::Message { fence_token, .. }
            | SessionLogEntry::ToolCalls { fence_token, .. } => *fence_token,
            SessionLogEntry::Cancel { fence_token }
            | SessionLogEntry::Error { fence_token, .. }
            | SessionLogEntry::TurnEnd { fence_token, .. } => Some(*fence_token),
            _ => None,
        }
    }
}

/// Highest fence token stamped on any worker-originated entry in the slice.
///
/// Used on resume to fail-safe: if a worker observes a fence greater than the
/// KV revision it currently holds, a newer worker has taken over and the
/// resuming worker must abort before writing.
pub fn max_worker_fence_token(entries: &[SessionLogEntry]) -> Option<u64> {
    entries.iter().filter_map(|e| e.fence_token()).max()
}

/// A single tool-call result as persisted in the session log. Matches
/// the corresponding `ToolCall` in the preceding `ToolCalls` entry by
/// `id` (or by position when `id` is absent).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<crate::message::MessageContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_agent: Option<SwitchAgentData>,
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
    pub compress_threshold: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub agent_variables: AgentVariables,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_instructions: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_fallbacks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_agent: Option<String>,
    #[serde(skip)]
    pub compaction_summary: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compressed_messages: Vec<Message>,
    pub messages: Vec<Message>,
    /// Maps an attachment reference (`cid:<sha256>`) to the relative filename
    /// of the blob stored under the session's `{id}.attachments/` directory.
    /// (Historically this mapped `sha256(data_uri)` to a source file path.)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub data_urls: HashMap<String, String>,

    #[serde(skip)]
    pub model: Model,
    #[serde(skip)]
    pub agent_prompt: String,
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub dirty: bool,
    #[serde(skip)]
    pub compressing: bool,
    #[serde(skip)]
    pub title: Option<String>,
    #[serde(skip)]
    pub titling: bool,
    #[serde(skip)]
    pub title_last_updated_tokens: usize,
    #[serde(skip)]
    pub log_entry_count: usize,
    #[serde(skip)]
    pub tokens: usize,
    #[serde(skip)]
    pub completion_usage: CompletionTokenUsage,
    /// Non-fatal problems encountered while reconstructing an append-only log.
    /// Callers should surface these while continuing to render the recovered
    /// transcript.
    #[serde(skip)]
    pub replay_warnings: Vec<String>,
    #[serde(skip)]
    pub runtime: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
}

impl Session {
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.compressed_messages.is_empty()
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn agent_name(&self) -> Option<&str> {
        self.agent_name.as_deref()
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    /// Returns sequence number that next appended entry will receive.
    /// This is equal to number of YAML documents currently in log file.
    pub fn next_seq(&self) -> usize {
        self.log_entry_count
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
            .with_context(|| format!("Unable to show info about session '{}'", self.id))?;
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

    pub fn set_agent(&mut self, agent: &AgentConfig) -> anyhow::Result<()> {
        // Render the template first so a failure leaves session state unchanged.
        let new_prompt = agent.interpolated_instructions()?;
        let new_variables = agent.variables().clone();
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
        self.agent_prompt = new_prompt;
        self.agent_variables = new_variables;
        self.agent_instructions = agent.instructions_template().to_string();
        self.dirty = true;
        self.update_tokens();
        Ok(())
    }

    pub fn sync_agent(&mut self, agent: &AgentConfig) -> anyhow::Result<()> {
        // Render the template first so a failure leaves session state unchanged.
        let new_prompt = agent.interpolated_instructions()?;
        let new_variables = agent.variables().clone();
        self.agent_name = if agent.name().is_empty() {
            None
        } else {
            Some(agent.name().to_string())
        };
        self.agent_prompt = new_prompt;
        self.agent_variables = new_variables;
        self.agent_instructions = agent.instructions_template().to_string();
        Ok(())
    }

    pub fn agent_variables(&self) -> &AgentVariables {
        &self.agent_variables
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

    pub fn need_generate_title(&self, threshold: usize) -> bool {
        if self.titling {
            return false;
        }
        if threshold == 0 {
            return false;
        }
        // First title: as soon as the session has any content and no title yet,
        // generate one (don't wait for a full threshold of growth). A manually
        // set title freezes `title_last_updated_tokens` at `usize::MAX`, so this
        // branch does not fire once a title exists.
        if self.title.is_none() {
            return self.tokens > 0;
        }
        // Subsequent regeneration: only after another `threshold` tokens of growth.
        self.tokens.saturating_sub(self.title_last_updated_tokens) >= threshold
    }

    pub fn titling(&self) -> bool {
        self.titling
    }

    pub fn set_titling(&mut self, v: bool) {
        self.titling = v;
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn set_title(&mut self, t: String) {
        self.title = Some(t);
    }

    pub fn title_last_updated_tokens(&self) -> usize {
        self.title_last_updated_tokens
    }

    pub fn set_title_last_updated_tokens(&mut self, t: usize) {
        self.title_last_updated_tokens = t;
    }

    pub fn guard_empty(&self) -> Result<()> {
        if !self.is_empty() {
            bail!("Cannot perform this operation because the session has messages, please `.empty session` first.");
        }
        Ok(())
    }
}

impl Session {
    pub fn to_agent_config(&self) -> Result<AgentConfig> {
        let agent_name = self.agent_name.as_deref().unwrap_or(TEMP_AGENT_NAME);
        let prompt = if self.agent_instructions.is_empty() {
            self.agent_prompt.as_str()
        } else {
            self.agent_instructions.as_str()
        };
        let mut config = AgentConfig::from_prompt(prompt);
        config.set_name(agent_name);
        config.set_model(self.model.clone());
        config.set_temperature(self.temperature);
        config.set_top_p(self.top_p);
        config.set_use_tools(self.use_tools.clone());
        config.set_model_fallbacks(self.model_fallbacks.clone());
        config.set_compaction_agent(self.compaction_agent.clone());
        config.set_shared_variables(self.agent_variables.clone());
        Ok(config)
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

    #[test]
    fn session_log_entry_message_timestamp_serde_round_trip() {
        let entry = SessionLogEntry::Message {
            id: None,
            role: MessageRole::User,
            content: MessageContent::Text("hello".to_string()),
            timestamp: Some(Utc::now()),
            fence_token: None,
        };

        let yaml = serde_yaml::to_string(&entry).unwrap();
        // Verify timestamp is serialized
        assert!(yaml.contains("timestamp:"));

        let round_tripped: SessionLogEntry = serde_yaml::from_str(&yaml).unwrap();

        match round_tripped {
            SessionLogEntry::Message {
                role,
                content,
                timestamp,
                ..
            } => {
                assert_eq!(role, MessageRole::User);
                assert!(timestamp.is_some());
                match content {
                    MessageContent::Text(text) => assert_eq!(text, "hello"),
                    _ => panic!("expected text content"),
                }
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn session_log_entry_message_without_timestamp_deserializes() {
        // Old logs without timestamp field should deserialize successfully
        let yaml = "type: message\nrole: user\ncontent: hello\n";
        let entry: SessionLogEntry = serde_yaml::from_str(yaml).unwrap();

        match entry {
            SessionLogEntry::Message {
                role,
                content,
                timestamp,
                ..
            } => {
                assert_eq!(role, MessageRole::User);
                assert!(timestamp.is_none());
                match content {
                    MessageContent::Text(text) => assert_eq!(text, "hello"),
                    _ => panic!("expected text content"),
                }
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[test]
    fn session_log_entry_tool_calls_timestamp_serde_round_trip() {
        let entry = SessionLogEntry::ToolCalls {
            text: "doing work".to_string(),
            thought: None,
            calls: vec![],
            timestamp: Some(Utc::now()),
            fence_token: None,
        };

        let yaml = serde_yaml::to_string(&entry).unwrap();
        assert!(yaml.contains("timestamp:"));

        let round_tripped: SessionLogEntry = serde_yaml::from_str(&yaml).unwrap();
        match round_tripped {
            SessionLogEntry::ToolCalls { timestamp, .. } => {
                assert!(timestamp.is_some());
            }
            other => panic!("expected tool_calls, got {other:?}"),
        }
    }

    #[test]
    fn session_log_entry_tool_results_timestamp_serde_round_trip() {
        let entry = SessionLogEntry::ToolResults {
            results: vec![],
            timestamp: Some(Utc::now()),
        };

        let yaml = serde_yaml::to_string(&entry).unwrap();
        assert!(yaml.contains("timestamp:"));

        let round_tripped: SessionLogEntry = serde_yaml::from_str(&yaml).unwrap();
        match round_tripped {
            SessionLogEntry::ToolResults { timestamp, .. } => {
                assert!(timestamp.is_some());
            }
            other => panic!("expected tool_results, got {other:?}"),
        }
    }

    #[test]
    fn to_agent_config_preserves_prompt_starting_with_frontmatter_delimiter() {
        let mut session = Session::default();
        session.agent_name = Some("resume-agent".to_string());
        session.agent_instructions = "---\nModel={{ agent.model }}".to_string();
        session.model = Model::new("openai", "gpt-4o");
        session.model_id = session.model.id();

        let config = session.to_agent_config().unwrap();
        assert_eq!(config.name(), "resume-agent");
        let rendered = config.system_text().unwrap();
        assert_eq!(rendered, "---\nModel=openai:gpt-4o");
    }

    #[test]
    fn session_log_entry_edit_entries_serde_round_trip() {
        let entry = SessionLogEntry::EditEntries {
            from: 3,
            to: 5,
            replacements: vec![
                "type: message
role: user
content: replacement one
"
                .to_string(),
                "type: message
role: assistant
content: replacement two
"
                .to_string(),
            ],
        };

        let yaml = serde_yaml::to_string(&entry).unwrap();
        let round_tripped: SessionLogEntry = serde_yaml::from_str(&yaml).unwrap();

        match round_tripped {
            SessionLogEntry::EditEntries {
                from,
                to,
                replacements,
            } => {
                assert_eq!(from, 3);
                assert_eq!(to, 5);
                assert_eq!(
                    replacements,
                    vec![
                        "type: message
role: user
content: replacement one
"
                        .to_string(),
                        "type: message
role: assistant
content: replacement two
"
                        .to_string(),
                    ]
                );
            }
            other => panic!("expected edit_entries, got {other:?}"),
        }
    }

    #[test]
    fn session_log_entry_rewind_serde_round_trip() {
        let entry = SessionLogEntry::Rewind { after_seq: 7 };

        let yaml = serde_yaml::to_string(&entry).unwrap();
        let round_tripped: SessionLogEntry = serde_yaml::from_str(&yaml).unwrap();

        match round_tripped {
            SessionLogEntry::Rewind { after_seq } => assert_eq!(after_seq, 7),
            other => panic!("expected rewind, got {other:?}"),
        }
    }

    #[test]
    fn session_log_entry_removed_set_pending_message_deserializes_as_unknown() {
        let yaml = "type: set_pending_message\ntext: pending assistant text\n";
        let round_tripped: SessionLogEntry = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(round_tripped, SessionLogEntry::Unknown));
    }

    #[test]
    fn session_log_entry_cancel_serde_round_trip() {
        let entry = SessionLogEntry::Cancel { fence_token: 42 };

        let yaml = serde_yaml::to_string(&entry).unwrap();
        let round_tripped: SessionLogEntry = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(yaml, "type: cancel\nfence_token: 42\n");
        match round_tripped {
            SessionLogEntry::Cancel { fence_token } => {
                assert_eq!(fence_token, 42);
            }
            other => panic!("expected cancel, got {other:?}"),
        }
    }

    #[test]
    fn session_log_entry_error_serde_round_trip() {
        let entry = SessionLogEntry::Error {
            message: "Template error in agent 'sisyphus': undefined value".to_string(),
            fence_token: 7,
            timestamp: None,
        };

        let yaml = serde_yaml::to_string(&entry).unwrap();
        let round_tripped: SessionLogEntry = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(round_tripped.fence_token(), Some(7));
        match round_tripped {
            SessionLogEntry::Error { message, .. } => {
                assert!(message.contains("undefined value"));
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn session_log_entry_none_fence_token_is_not_serialized() {
        let assistant_message = SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("hello".to_string()),
            timestamp: None,
            fence_token: None,
        };
        let tool_calls = SessionLogEntry::ToolCalls {
            text: "working".to_string(),
            thought: None,
            calls: vec![],
            timestamp: None,
            fence_token: None,
        };

        let assistant_yaml = serde_yaml::to_string(&assistant_message).unwrap();
        let tool_calls_yaml = serde_yaml::to_string(&tool_calls).unwrap();

        assert!(!assistant_yaml.contains("fence_token:"));
        assert!(!tool_calls_yaml.contains("fence_token:"));
    }

    #[test]
    fn set_fence_token_stamps_only_message_and_tool_calls() {
        let mut msg = SessionLogEntry::Message {
            id: None,
            role: MessageRole::Assistant,
            content: MessageContent::Text("x".to_string()),
            timestamp: None,
            fence_token: None,
        };
        msg.set_fence_token(7);
        assert_eq!(msg.fence_token(), Some(7));

        let mut calls = SessionLogEntry::ToolCalls {
            text: String::new(),
            thought: None,
            calls: vec![],
            timestamp: None,
            fence_token: None,
        };
        calls.set_fence_token(9);
        assert_eq!(calls.fence_token(), Some(9));
    }

    #[test]
    fn cancel_entry_reports_its_fence_token() {
        let cancel = SessionLogEntry::Cancel { fence_token: 5 };
        assert_eq!(cancel.fence_token(), Some(5));
    }

    #[test]
    fn max_worker_fence_token_finds_highest() {
        let entries = vec![
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("u".to_string()),
                timestamp: None,
                fence_token: None,
            },
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text("a".to_string()),
                timestamp: None,
                fence_token: Some(3),
            },
            SessionLogEntry::ToolCalls {
                text: String::new(),
                thought: None,
                calls: vec![],
                timestamp: None,
                fence_token: Some(8),
            },
            SessionLogEntry::Cancel { fence_token: 6 },
        ];
        assert_eq!(max_worker_fence_token(&entries), Some(8));
        assert_eq!(max_worker_fence_token(&[]), None);
    }

    #[test]
    fn session_log_entry_unknown_type_deserializes_to_unknown() {
        let yaml = "type: future_variant
field: value
";
        let entry: SessionLogEntry = serde_yaml::from_str(yaml).unwrap();

        assert!(matches!(entry, SessionLogEntry::Unknown));
    }

    #[test]
    fn need_generate_title_uses_titling_threshold_and_token_delta() {
        // First-title branch: no title yet and some content -> generate.
        let mut session = Session {
            tokens: 20,
            title_last_updated_tokens: 10,
            titling: true,
            ..Default::default()
        };

        // While titling is in progress, never trigger.
        assert!(!session.need_generate_title(5));

        session.titling = false;
        // Threshold 0 disables generation entirely.
        assert!(!session.need_generate_title(0));
        // No title yet + tokens > 0 -> first title fires regardless of delta.
        assert!(session.need_generate_title(10));
        assert!(session.need_generate_title(usize::MAX));

        // Empty session (no tokens) never generates a first title.
        let empty = Session::default();
        assert!(!empty.need_generate_title(10));

        // Regeneration branch: once a title exists, only token growth beyond the
        // threshold triggers again.
        let mut titled = Session {
            tokens: 20,
            title_last_updated_tokens: 10,
            title: Some("Existing title".to_string()),
            ..Default::default()
        };
        assert!(titled.need_generate_title(10)); // delta 10 >= 10
        assert!(!titled.need_generate_title(11)); // delta 10 < 11

        // A frozen (manual) title uses usize::MAX and never regenerates.
        titled.title_last_updated_tokens = usize::MAX;
        assert!(!titled.need_generate_title(1));
    }

    #[test]
    fn set_model_fallbacks_updates_session_and_marks_dirty() {
        let mut session = Session::default();

        assert!(session.model_fallbacks().is_empty());

        session.set_model_fallbacks(vec!["anthropic:claude".to_string()]);

        assert_eq!(session.model_fallbacks(), &["anthropic:claude".to_string()]);
        assert!(session.dirty);
    }

    #[test]
    fn tool_output_deserializes_without_markdown_field() {
        let serialized = json!({
            "id": "tool-1",
            "name": "read_history",
            "output": {"ok": true},
            "content": [],
            "switch_agent": null
        });

        let decoded: ToolOutput = serde_json::from_value(serialized).unwrap();
        assert_eq!(decoded.id.as_deref(), Some("tool-1"));
        assert_eq!(decoded.name, "read_history");
        assert_eq!(decoded.output, json!({"ok": true}));
        assert!(decoded.markdown.is_none());
        assert!(decoded.content.is_empty());
        assert!(decoded.switch_agent.is_none());
    }
}
