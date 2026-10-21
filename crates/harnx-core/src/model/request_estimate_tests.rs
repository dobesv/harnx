use super::*;
use crate::message::{ImageUrl, MessageRole};
use crate::tool::{ToolCall, ToolDeclaration, ToolResult};
use serde_json::json;

fn image() -> MessageContentPart {
    MessageContentPart::ImageUrl {
        image_url: ImageUrl {
            url: "cid:image".into(),
        },
    }
}

#[test]
fn request_estimate_counts_images_in_messages_and_tool_results() {
    let result = ToolResult {
        content: vec![image()],
        ..ToolResult::new(
            ToolCall::new("read".into(), json!({}), None, None),
            json!({}),
        )
    };
    let messages = vec![
        Message::new(MessageRole::User, MessageContent::Array(vec![image()])),
        Message::new(
            MessageRole::Tool,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![result],
                String::new(),
                None,
            )),
        ),
    ];
    let model = Model::default();
    let tokens = model.messages_tokens(&messages) + 3000 + estimate_token_length("[]");
    assert_eq!(
        model.estimate_request_tokens(&messages, &[]),
        tokens + tokens.div_ceil(10)
    );
}

#[test]
fn request_estimate_counts_system_tools_and_rounds_margin_up() {
    let tools: Vec<ToolDeclaration> = serde_json::from_value(json!([{
        "name": "read", "description": "description ".repeat(500), "parameters": {"type": "object"}
    }]))
    .unwrap();
    let messages = vec![Message::new(
        MessageRole::System,
        MessageContent::Text("instructions ".repeat(100)),
    )];
    let model = Model::default();
    let tokens = model.messages_tokens(&messages)
        + estimate_token_length(&serde_json::to_string(&tools).unwrap());
    assert_eq!(
        model.estimate_request_tokens(&messages, &tools),
        tokens + tokens.div_ceil(10)
    );
    assert!(model.estimate_request_tokens(&messages, &tools) > model.messages_tokens(&messages));
}
