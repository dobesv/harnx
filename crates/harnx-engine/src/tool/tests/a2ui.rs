use super::*;

use harnx_client::openai::openai_build_chat_completions_body;
use harnx_client::ChatCompletionsData;
use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
use harnx_core::model::Model;

fn private_surface_block() -> Value {
    let surface = json!({
        "version": "1.0",
        "createSurface": {
            "surfaceId": "PRIVATE_SURFACE_ID",
            "components": [
                {"id": "root", "component": "Column", "children": ["label", "go"]},
                {"id": "label", "component": "Text", "text": "PRIVATE_VALID_TEXT"},
                {"id": "go", "component": "Button", "child": "label",
                 "action": {"event": {"name": "go"}}}
            ]
        }
    });
    json!({
        "type": "resource",
        "annotations": {"audience": ["user"]},
        "resource": {"uri": "a2ui://PRIVATE_RESOURCE_URI",
                     "mimeType": crate::a2ui::A2UI_MIME_TYPE, "text": surface.to_string()}
    })
}

fn mixed_private_output(invalid_first: bool, malformed_json: bool) -> Value {
    let valid = private_surface_block();
    let mut invalid = valid.clone();
    invalid["resource"]["text"] = if malformed_json {
        json!("PRIVATE_INVALID_JSON")
    } else {
        let mut surface: Value =
            serde_json::from_str(valid["resource"]["text"].as_str().unwrap()).unwrap();
        surface["createSurface"]["components"][0]["component"] = json!("PRIVATE_INVALID_COMPONENT");
        json!(surface.to_string())
    };
    let mut blocks = vec![valid, invalid];
    if invalid_first {
        blocks.reverse();
    }
    blocks.push(json!({"type": "text", "text": "public reply"}));
    json!({"content": blocks})
}

fn model_request(result: ToolResult) -> Value {
    let data = ChatCompletionsData {
        messages: vec![Message {
            role: MessageRole::Assistant,
            content: MessageContent::ToolCalls(MessageContentToolCalls::new(
                vec![result],
                String::new(),
                None,
            )),
            ..Default::default()
        }],
        temperature: None,
        top_p: None,
        functions: None,
        stream: false,
        attachments_dir: None,
    };
    openai_build_chat_completions_body(data, &Model::new("openai", "gpt-4o"))
}

fn assert_safe_model_result(result: ToolResult) {
    assert_eq!(result.output["is_error"], true);
    assert!(result.user_surfaces.is_empty());
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(!serialized.contains("PRIVATE_"), "{serialized}");
    let restored: ToolResult = serde_json::from_str(&serialized).unwrap();
    let request = model_request(result);
    assert_eq!(
        request,
        model_request(restored),
        "durable round-trip projection"
    );
    let wire = request.to_string();
    assert!(
        wire.contains("invalid user-audience a2ui surface"),
        "{wire}"
    );
    for private in [
        "PRIVATE_",
        "createSurface",
        "original_output",
        crate::a2ui::A2UI_MIME_TYPE,
    ] {
        assert!(!wire.contains(private), "{wire}");
    }
}

#[tokio::test]
async fn invalid_a2ui_sibling_never_reaches_final_model_request() {
    for invalid_first in [false, true] {
        for malformed_json in [false, true] {
            let ctx = test_context_with_emitters(
                vec![Arc::new(MockToolProvider::ok(
                    "surface_tool",
                    Duration::ZERO,
                    mixed_private_output(invalid_first, malformed_json),
                ))],
                |_, _| {},
                |_, output| assert!(!output.to_string().contains("PRIVATE_")),
                |_, _| {},
                |_, _| {},
            );
            let mut results = eval_tool_calls(
                &ctx,
                vec![test_call("surface_tool")],
                &create_abort_signal(),
            )
            .await
            .unwrap();
            assert_eq!(results.len(), 1);
            assert_safe_model_result(results.remove(0));
        }
    }
}

#[tokio::test]
async fn invalid_a2ui_sibling_never_reaches_replayed_model_request() {
    for invalid_first in [false, true] {
        for malformed_json in [false, true] {
            let ctx = test_context_with_emitters(
                vec![Arc::new(MockToolProvider::ok(
                    "replay_only",
                    Duration::ZERO,
                    mixed_private_output(invalid_first, malformed_json),
                ))],
                |_, _| {},
                |_, output| assert!(!output.to_string().contains("PRIVATE_")),
                |_, _| {},
                |_, _| {},
            );
            let call = test_call("replay_only");
            let result = replay_tool_call(
                &ctx,
                harnx_core::tool::ToolReplay {
                    session_id: "session",
                    tool_round: 1,
                    call: &call,
                    worker_id: None,
                    fence_token: None,
                    authorization: None,
                },
                &create_abort_signal(),
            )
            .await
            .unwrap()
            .unwrap();
            assert_safe_model_result(result);
        }
    }
}

#[tokio::test]
async fn invalid_a2ui_sibling_cannot_hide_in_malformed_image_diagnostic() {
    for replay in [false, true] {
        let mut output = mixed_private_output(false, false);
        output["content"].as_array_mut().unwrap().push(json!({
            "type": "resource_link", "uri": "cid:media:invalid", "mimeType": "image/png"
        }));
        let ctx = test_context(vec![Arc::new(MockToolProvider::ok(
            "replay_only",
            Duration::ZERO,
            output,
        ))]);
        let call = test_call("replay_only");
        let result = if replay {
            replay_tool_call(
                &ctx,
                harnx_core::tool::ToolReplay {
                    session_id: "session",
                    tool_round: 1,
                    call: &call,
                    worker_id: None,
                    fence_token: None,
                    authorization: None,
                },
                &create_abort_signal(),
            )
            .await
            .unwrap()
            .unwrap()
        } else {
            eval_tool_calls(&ctx, vec![call], &create_abort_signal())
                .await
                .unwrap()
                .remove(0)
        };
        assert_safe_model_result(result);
    }
}
