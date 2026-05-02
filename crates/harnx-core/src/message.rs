//! Chat message data types used across the client and engine. Pure data
//! types only — display helpers (`render_message_input`) and Model-aware
//! helpers (`patch_messages`) live in `harnx/src/client/message.rs` where
//! they have access to the terminal-color utilities and the `Model`
//! configuration type respectively.

use serde::{Deserialize, Serialize};

use crate::tool::ToolResult;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
    /// 0-based index of the YAML document in the session log that produced
    /// this message. Set during log replay; `None` for messages created
    /// during a live session (before they are persisted and reloaded).
    /// Never serialised — not sent to the LLM.
    #[serde(skip)]
    pub log_seq: Option<usize>,
    /// Stored log timestamp for message-like entries. Seq is positional-only;
    /// timestamp is persisted in YAML and propagated for transcript rendering.
    #[serde(skip)]
    pub log_timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            role: MessageRole::User,
            content: MessageContent::Text(String::new()),
            log_seq: None,
            log_timestamp: None,
        }
    }
}

impl Message {
    pub fn new(role: MessageRole, content: MessageContent) -> Self {
        Self {
            role,
            content,
            log_seq: None,
            log_timestamp: None,
        }
    }

    pub fn with_log_seq(mut self, seq: usize) -> Self {
        self.log_seq = Some(seq);
        self
    }

    pub fn with_log_timestamp(mut self, timestamp: chrono::DateTime<chrono::Utc>) -> Self {
        self.log_timestamp = Some(timestamp);
        self
    }

    pub fn merge_system(&mut self, system: MessageContent) {
        match (&mut self.content, system) {
            (MessageContent::Text(text), MessageContent::Text(system_text)) => {
                self.content = MessageContent::Array(vec![
                    MessageContentPart::Text { text: system_text },
                    MessageContentPart::Text {
                        text: text.to_string(),
                    },
                ])
            }
            (MessageContent::Array(list), MessageContent::Text(system_text)) => {
                list.insert(0, MessageContentPart::Text { text: system_text })
            }
            (MessageContent::Text(text), MessageContent::Array(mut system_list)) => {
                system_list.push(MessageContentPart::Text {
                    text: text.to_string(),
                });
                self.content = MessageContent::Array(system_list);
            }
            (MessageContent::Array(list), MessageContent::Array(mut system_list)) => {
                system_list.append(list);
                self.content = MessageContent::Array(system_list);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    Assistant,
    User,
    Tool,
}

#[allow(dead_code)]
impl MessageRole {
    pub fn is_system(&self) -> bool {
        matches!(self, MessageRole::System)
    }

    pub fn is_user(&self) -> bool {
        matches!(self, MessageRole::User)
    }

    pub fn is_assistant(&self) -> bool {
        matches!(self, MessageRole::Assistant)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Array(Vec<MessageContentPart>),
    // Note: This type is primarily for convenience and does not exist in OpenAI's API.
    ToolCalls(MessageContentToolCalls),
}

impl MessageContent {
    pub fn to_text(&self) -> String {
        match self {
            MessageContent::Text(text) => text.to_string(),
            MessageContent::Array(list) => {
                let mut parts = vec![];
                for item in list {
                    if let MessageContentPart::Text { text } = item {
                        parts.push(text.clone())
                    }
                }
                parts.join("\n\n")
            }
            MessageContent::ToolCalls(_) => String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageContentToolCalls {
    pub tool_results: Vec<ToolResult>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought: Option<String>,
    pub sequence: bool,
}

impl MessageContentToolCalls {
    pub fn new(tool_results: Vec<ToolResult>, text: String, thought: Option<String>) -> Self {
        Self {
            tool_results,
            text,
            thought,
            sequence: false,
        }
    }

    pub fn merge(&mut self, tool_results: Vec<ToolResult>, text: String, thought: Option<String>) {
        self.tool_results.extend(tool_results);
        if !text.is_empty() {
            if !self.text.is_empty() {
                self.text.push_str("\n\n");
            }
            self.text.push_str(&text);
        }
        if let Some(new_thought) = thought {
            if let Some(old_thought) = self.thought.as_mut() {
                old_thought.push_str("\n\n");
                old_thought.push_str(&new_thought);
            } else {
                self.thought = Some(new_thought);
            }
        }
        self.sequence = true;
    }
}

pub fn extract_system_message(messages: &mut Vec<Message>) -> Option<Vec<String>> {
    if messages[0].role.is_system() {
        let system_message = messages.remove(0);
        let parts = match system_message.content {
            MessageContent::Text(s) => vec![s],
            MessageContent::Array(list) => list
                .into_iter()
                .filter_map(|p| {
                    if let MessageContentPart::Text { text } = p {
                        Some(text)
                    } else {
                        None
                    }
                })
                .collect(),
            MessageContent::ToolCalls(_) => vec![],
        };
        if parts.is_empty() {
            return None;
        }
        return Some(parts);
    }
    None
}
