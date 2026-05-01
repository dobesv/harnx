use super::{
    Message, MessageContent, MessageContentPart, MessageContentToolCalls, MessageRole, Model,
};

use crate::utils::{dimmed_text, multiline_text};

pub fn render_message_input(
    content: &MessageContent,
    resolve_url_fn: impl Fn(&str) -> String,
    agent_info: &Option<(String, Vec<String>)>,
) -> String {
    match content {
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
                    log_seq: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::message::extract_system_message;

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
