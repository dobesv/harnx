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

fn strip_image_parts(messages: &mut [Message]) -> usize {
    let mut stripped_images = 0usize;

    for message in messages.iter_mut() {
        match &mut message.content {
            MessageContent::ToolCalls(tool_calls) => {
                stripped_images += strip_tool_call_image_parts(tool_calls);
            }
            MessageContent::Array(parts) => {
                stripped_images += strip_content_part_images(parts);
            }
            _ => {}
        }
    }

    stripped_images
}

fn strip_tool_call_image_parts(tool_calls: &mut MessageContentToolCalls) -> usize {
    tool_calls
        .tool_results
        .iter_mut()
        .map(|tool_result| strip_content_part_images(&mut tool_result.content))
        .sum()
}

fn strip_content_part_images(parts: &mut Vec<MessageContentPart>) -> usize {
    let before = parts.len();
    parts.retain(|part| !matches!(part, MessageContentPart::ImageUrl { .. }));
    before - parts.len()
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
                    id: None,
                    log_seq: None,
                    log_timestamp: None,
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
    if !model.supports_vision() {
        let stripped_images = strip_image_parts(messages);
        if stripped_images > 0 {
            log::warn!(
                "model '{}' lacks vision support; stripped {} image(s)",
                model.id(),
                stripped_images
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::{
        message::{extract_system_message, ImageUrl},
        tool::{ToolCall, ToolResult},
    };

    fn tool_result_with_content(content: Vec<MessageContentPart>) -> ToolResult {
        ToolResult {
            call: ToolCall::new(
                "read_media".to_string(),
                json!({ "path": "image.png" }),
                None,
                None,
            ),
            output: json!({ "status": "ok" }),
            content,
            switch_agent: None,
        }
    }
    use serde_json::json;

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

    #[test]
    fn patch_messages_strips_tool_result_images_for_non_vision_model() {
        let mut messages = vec![Message::new(
            MessageRole::Assistant,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![tool_result_with_content(vec![
                    MessageContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: "file:///image.png".to_string(),
                        },
                    },
                ])],
                "tool output".to_string(),
                None,
            )),
        )];
        let model = Model::new("test", "test-model");

        patch_messages(&mut messages, &model);

        match &messages[0].content {
            MessageContent::ToolCalls(tool_calls) => {
                assert!(tool_calls.tool_results[0].content.is_empty());
            }
            other => panic!("Expected ToolCalls, got {:?}", other),
        }
    }

    #[test]
    fn patch_messages_keeps_tool_result_images_for_vision_model() {
        let mut messages = vec![Message::new(
            MessageRole::Assistant,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![tool_result_with_content(vec![
                    MessageContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: "file:///image.png".to_string(),
                        },
                    },
                ])],
                "tool output".to_string(),
                None,
            )),
        )];
        let mut model = Model::new("test", "test-model");
        model.data_mut().supports_vision = true;

        patch_messages(&mut messages, &model);

        match &messages[0].content {
            MessageContent::ToolCalls(tool_calls) => {
                assert_eq!(tool_calls.tool_results[0].content.len(), 1);
                assert!(matches!(
                    tool_calls.tool_results[0].content[0],
                    MessageContentPart::ImageUrl { .. }
                ));
            }
            other => panic!("Expected ToolCalls, got {:?}", other),
        }
    }

    #[test]
    fn patch_messages_strips_only_image_parts_for_non_vision_model() {
        let mut messages = vec![Message::new(
            MessageRole::Assistant,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![tool_result_with_content(vec![
                    MessageContentPart::Text {
                        text: "caption".to_string(),
                    },
                    MessageContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: "file:///image.png".to_string(),
                        },
                    },
                ])],
                "tool output".to_string(),
                None,
            )),
        )];
        let model = Model::new("test", "test-model");

        patch_messages(&mut messages, &model);

        match &messages[0].content {
            MessageContent::ToolCalls(tool_calls) => {
                assert_eq!(tool_calls.tool_results[0].content.len(), 1);
                assert!(matches!(
                    &tool_calls.tool_results[0].content[0],
                    MessageContentPart::Text { text } if text == "caption"
                ));
            }
            other => panic!("Expected ToolCalls, got {:?}", other),
        }
    }

    #[test]
    fn patch_messages_strips_user_images_for_non_vision_model() {
        let mut messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Array(vec![
                MessageContentPart::Text {
                    text: "describe this".to_string(),
                },
                MessageContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "file:///image.png".to_string(),
                    },
                },
            ]),
        )];
        let model = Model::new("test", "test-model");

        patch_messages(&mut messages, &model);

        match &messages[0].content {
            MessageContent::Array(parts) => {
                assert_eq!(parts.len(), 1);
                assert!(matches!(
                    &parts[0],
                    MessageContentPart::Text { text } if text == "describe this"
                ));
            }
            other => panic!("Expected Array, got {:?}", other),
        }
    }

    #[test]
    fn patch_messages_keeps_user_images_for_vision_model() {
        let mut messages = vec![Message::new(
            MessageRole::User,
            MessageContent::Array(vec![
                MessageContentPart::Text {
                    text: "describe this".to_string(),
                },
                MessageContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "file:///image.png".to_string(),
                    },
                },
            ]),
        )];
        let mut model = Model::new("test", "test-model");
        model.data_mut().supports_vision = true;

        patch_messages(&mut messages, &model);

        match &messages[0].content {
            MessageContent::Array(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(
                    &parts[0],
                    MessageContentPart::Text { text } if text == "describe this"
                ));
                assert!(matches!(&parts[1], MessageContentPart::ImageUrl { .. }));
            }
            other => panic!("Expected Array, got {:?}", other),
        }
    }
}
