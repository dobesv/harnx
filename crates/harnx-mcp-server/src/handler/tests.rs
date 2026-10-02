use super::*;
use serde_json::json;

#[test]
fn plain_values_become_text() {
    for value in [
        json!("text without quotes"),
        json!({"key": [1, true]}),
        json!(null),
        json!(42),
        json!(["array"]),
    ] {
        let expected = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| serde_json::to_string_pretty(&value).unwrap());
        let result = tool_result(Ok(value.into()));
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            serde_json::to_value(&result).unwrap()["content"],
            json!([{"type":"text","text":expected}])
        );
    }
}

#[test]
fn all_mcp_content_blocks_and_result_metadata_survive() {
    let envelope = json!({
        "content": [
            {"type":"text", "text":"markdown", "annotations":{"audience":["user"], "priority":0.5}},
            {"type":"image", "data":"aW1hZ2U=", "mimeType":"image/png"},
            {"type":"audio", "data":"YXVkaW8=", "mimeType":"audio/wav"},
            {"type":"resource", "resource":{"uri":"file:///result.txt", "mimeType":"text/plain", "text":"embedded"}},
            {"type":"resource", "resource":{"uri":"file:///result.bin", "mimeType":"application/octet-stream", "blob":"YmluYXJ5"}},
            {"type":"resource_link", "uri":"https://example.com/result", "name":"result", "mimeType":"text/plain"}
        ],
        "structuredContent":{"key":"value"}, "isError":true, "_meta":{"custom":"metadata"}
    });
    let expected: CallToolResult = serde_json::from_value(envelope).unwrap();
    let result = tool_result(Ok(serde_json::to_value(&expected).unwrap().into()));
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn empty_success_and_protocol_error_envelopes_are_preserved() {
    for envelope in [
        json!({"content":[], "isError":false}),
        json!({"content":[{"type":"text", "text":"bad arguments"}], "isError":true}),
    ] {
        let expected: CallToolResult = serde_json::from_value(envelope.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(tool_result(Ok(envelope.into()))).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}

#[test]
fn both_provider_error_kinds_become_tool_errors_with_context() {
    for fatal in [false, true] {
        let error = anyhow::anyhow!("root cause").context("dispatch failed");
        let result = tool_result(Err(if fatal {
            ToolError::Fatal(error)
        } else {
            ToolError::Recoverable(error)
        }));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            serde_json::to_value(result).unwrap()["content"],
            json!([{"type":"text", "text":"dispatch failed: root cause"}])
        );
    }
}
