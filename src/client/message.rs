use super::Model;

use crate::{multiline_text, tool::ToolResult, utils::dimmed_text};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
}

impl Default for Message {
    fn default() -> Self {
        Self {
            role: MessageRole::User,
            content: MessageContent::Text(String::new()),
        }
    }
}

impl Message {
    pub fn new(role: MessageRole, content: MessageContent) -> Self {
        Self { role, content }
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
    pub fn render_input(
        &self,
        resolve_url_fn: impl Fn(&str) -> String,
        agent_info: &Option<(String, Vec<String>)>,
    ) -> String {
        match self {
            MessageContent::Text(text) => multiline_text(text),
            MessageContent::Array(list) => {
                let (mut concated_text, mut files) = (String::new(), vec![]);
                for item in list {
                    match item {
                        MessageContentPart::Text { text } => {
                            concated_text = format!("{concated_text} {text}")
                        }
                        MessageContentPart::ImageUrl { image_url } => {
                            files.push(resolve_url_fn(&image_url.url))
                        }
                    }
                }
                if !concated_text.is_empty() {
                    concated_text = format!(" -- {}", multiline_text(&concated_text))
                }
                format!(".file {}{}", files.join(" "), concated_text)
            }
            MessageContent::ToolCalls(MessageContentToolCalls {
                tool_results, text, ..
            }) => {
                let mut lines = vec![];
                if !text.is_empty() {
                    lines.push(text.clone())
                }
                for tool_result in tool_results {
                    let mut parts = vec!["Call".to_string()];
                    if let Some((agent_name, functions)) = agent_info {
                        if functions.contains(&tool_result.call.name) {
                            parts.push(agent_name.clone())
                        }
                    }
                    parts.push(tool_result.call.name.clone());
                    parts.push(tool_result.call.arguments.to_string());
                    lines.push(dimmed_text(&parts.join(" ")));
                }
                lines.join("\n")
            }
        }
    }

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

    pub fn merge(
        &mut self,
        tool_results: Vec<ToolResult>,
        _text: String,
        thought: Option<String>,
    ) {
        self.tool_results.extend(tool_results);
        self.text.clear();
        self.thought = thought;
        self.sequence = true;
    }
}

pub fn patch_messages(messages: &mut Vec<Message>, model: &Model) {
    if messages.is_empty() {
        return;
    }
    if let Some(prefix) = model.system_prompt_prefix() {
        let prefix_content = MessageContent::Array(
            prefix
                .iter()
                .map(|s| MessageContentPart::Text {
                    text: s.to_string(),
                })
                .collect(),
        );
        if messages[0].role.is_system() {
            messages[0].merge_system(prefix_content);
        } else {
            messages.insert(
                0,
                Message {
                    role: MessageRole::System,
                    content: prefix_content,
                },
            );
        }
    }
    if model.no_system_message() && messages[0].role.is_system() {
        let system_message = messages.remove(0);
        if let (Some(message), system) = (messages.get_mut(0), system_message.content) {
            message.merge_system(system);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_system_message_text_returns_vec() {
        let mut messages = vec![
            Message::new(
                MessageRole::System,
                MessageContent::Text("Be helpful".to_string()),
            ),
            Message::new(MessageRole::User, MessageContent::Text("Hello".to_string())),
        ];
        let result = extract_system_message(&mut messages);
        assert_eq!(result, Some(vec!["Be helpful".to_string()]));
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn extract_system_message_array_returns_separate_parts() {
        let mut messages = vec![
            Message::new(
                MessageRole::System,
                MessageContent::Array(vec![
                    MessageContentPart::Text {
                        text: "identity".to_string(),
                    },
                    MessageContentPart::Text {
                        text: "extra".to_string(),
                    },
                    MessageContentPart::Text {
                        text: "Be helpful".to_string(),
                    },
                ]),
            ),
            Message::new(MessageRole::User, MessageContent::Text("Hello".to_string())),
        ];
        let result = extract_system_message(&mut messages);
        assert_eq!(
            result,
            Some(vec![
                "identity".to_string(),
                "extra".to_string(),
                "Be helpful".to_string(),
            ])
        );
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn extract_system_message_none_when_no_system() {
        let mut messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Text("Hello".to_string()),
        )];
        let result = extract_system_message(&mut messages);
        assert_eq!(result, None);
    }

    #[test]
    fn patch_messages_builds_array_from_prefix() {
        let mut messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Text("Hello".to_string()),
        )];
        let mut model = Model::new("test", "test-model");
        model.data_mut().system_prompt_prefix =
            Some(vec!["identity".to_string(), "extra".to_string()]);

        patch_messages(&mut messages, &model);

        assert_eq!(messages.len(), 2);
        assert!(messages[0].role.is_system());
        match &messages[0].content {
            MessageContent::Array(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(
                    matches!(&parts[0], MessageContentPart::Text { text } if text == "identity")
                );
                assert!(matches!(&parts[1], MessageContentPart::Text { text } if text == "extra"));
            }
            other => panic!("Expected Array, got {:?}", other),
        }
    }

    #[test]
    fn patch_messages_merges_prefix_with_existing_system() {
        let mut messages = vec![
            Message::new(
                MessageRole::System,
                MessageContent::Text("Be helpful".to_string()),
            ),
            Message::new(MessageRole::User, MessageContent::Text("Hello".to_string())),
        ];
        let mut model = Model::new("test", "test-model");
        model.data_mut().system_prompt_prefix =
            Some(vec!["identity".to_string(), "extra".to_string()]);

        patch_messages(&mut messages, &model);

        assert_eq!(messages.len(), 2);
        assert!(messages[0].role.is_system());
        match &messages[0].content {
            MessageContent::Array(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(
                    matches!(&parts[0], MessageContentPart::Text { text } if text == "identity")
                );
                assert!(matches!(&parts[1], MessageContentPart::Text { text } if text == "extra"));
                assert!(
                    matches!(&parts[2], MessageContentPart::Text { text } if text == "Be helpful")
                );
            }
            other => panic!("Expected Array, got {:?}", other),
        }
    }
}
