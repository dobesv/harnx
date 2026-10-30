use super::*;
use harnx_core::{
    message::{ImageUrl, MessageContent, MessageContentPart, MessageContentToolCalls, MessageRole},
    tool::{ToolCall, ToolResult},
};
use serde_json::json;

fn image() -> MessageContentPart {
    MessageContentPart::ImageUrl {
        image_url: ImageUrl {
            url: "data:image/png;base64,AQID".into(),
        },
    }
}

#[test]
fn rejects_images_in_user_messages_and_tool_results_without_stripping_them() {
    let mut result = ToolResult::new(
        ToolCall::new("read".into(), json!({}), Some("call".into()), None),
        json!({"ok": true}),
    );
    result.content = vec![image()];
    let messages = [
        Message::new(MessageRole::User, MessageContent::Array(vec![image()])),
        Message::new(
            MessageRole::Assistant,
            MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![result],
                String::new(),
                None,
            )),
        ),
    ];
    let mut model = Model::new("mock", "vision");
    for message in &messages {
        let error = guard_images(&model, std::slice::from_ref(message)).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ModelFallbackReason>(),
            Some(&ModelFallbackReason::IncompatibleInput)
        );
        assert_eq!(request_image_count(&message.content), 1);
    }
    model.data_mut().supports_vision = true;
    guard_images(&model, &messages).unwrap();
}

#[test]
fn request_budget_counts_tool_schemas_and_preserves_typed_overflow_for_recovery() {
    let mut model = Model::new("mock", "small");
    model.data_mut().max_input_tokens = Some(100);
    let messages = vec![Message::new(
        MessageRole::User,
        MessageContent::Text("hi".into()),
    )];
    guard_request_size(&model, &messages, &[]).unwrap();
    let tools = vec![serde_json::from_value(json!({"name": "read", "description": "schema documentation ".repeat(100), "parameters": {"type": "object"}})).unwrap()];
    let error = guard_request_size(&model, &messages, &tools).unwrap_err();
    assert!(error
        .downcast_ref::<LlmError>()
        .unwrap()
        .is_context_overflow());
    model.data_mut().max_input_tokens = Some(10_000);
    guard_request_size(&model, &messages, &tools).unwrap();
}

#[test]
fn estimated_input_limit_is_exclusive_as_in_the_previous_guard() {
    let mut model = Model::new("mock", "boundary");
    let messages = vec![Message::new(
        MessageRole::User,
        MessageContent::Text("hello".into()),
    )];
    let tokens = model
        .estimate_request_tokens(&messages, &[])
        .max(model.total_tokens(&messages) + 2);
    model.data_mut().max_input_tokens = Some(tokens + 1);
    guard_request_size(&model, &messages, &[]).unwrap();
    model.data_mut().max_input_tokens = Some(tokens);
    let error = guard_request_size(&model, &messages, &[]).unwrap_err();
    assert!(error.to_string().contains("meets or exceeds"));
}
