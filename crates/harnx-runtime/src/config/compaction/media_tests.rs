use super::{render_tool_output, truncate_middle};
use harnx_core::session::ToolOutput;
use serde_json::{json, Value};

fn output(value: Value) -> ToolOutput {
    ToolOutput {
        id: None,
        name: "media".into(),
        output: value,
        markdown: None,
        content: vec![],
        switch_agent: None,
    }
}

fn assert_media_marker(block: Value, expected: &str) {
    let out = output(json!({"content": [block]}));
    let original = out.output.clone();
    let rendered = render_tool_output(&out, usize::MAX);
    let sanitized: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(sanitized["content"][0]["data"], expected);
    assert_eq!(
        out.output, original,
        "compaction must not change stored media"
    );
}

#[test]
fn tier_three_replaces_bare_image_and_audio_data_with_decoded_byte_counts() {
    let cases = [
        (
            json!({"type": "image", "mimeType": "image/png", "data": "AQIDBA=="}),
            "[image/png data: 4 bytes]",
        ),
        (
            json!({"type": "audio", "mime_type": "audio/wav", "data": "AQI="}),
            "[audio/wav data: 2 bytes]",
        ),
        (
            json!({"type": "audio", "mimeType": "audio/ogg", "data": "AQ-_"}),
            "[audio/ogg data: 3 bytes]",
        ),
        (
            json!({"type": "image", "mimeType": "image/png", "data": "AQID\r\nBA=="}),
            "[image/png data: 4 bytes]",
        ),
        (
            json!({"type": "image", "data": "AQID"}),
            "[image data: 3 bytes]",
        ),
        (
            json!({"type": "audio", "data": ""}),
            "[audio data: 0 bytes]",
        ),
    ];
    for (block, expected) in cases {
        assert_media_marker(block, expected);
    }
}

#[test]
fn tier_three_preserves_redacted_media_references_and_non_media_data() {
    let value = json!({"content": [
        {"type": "image", "mime_type": "image/png", "data": "<image: image/png, 8 base64 chars>"},
        {"type": "audio", "mimeType": "audio/wav", "data": "[audio/wav data: 2 bytes]"},
        {"type": "image", "data": "cid:media:test/image"},
        {"type": "image", "data": "https://example.com/image.png"},
        {"type": "audio", "data": null},
        {"type": "image"},
        {"type": "text", "data": "AQIDBA=="}
    ]});
    let rendered = render_tool_output(&output(value.clone()), usize::MAX);
    assert_eq!(serde_json::from_str::<Value>(&rendered).unwrap(), value);
}

#[test]
fn tier_three_sanitizes_nested_media_and_inline_urls() {
    let out = output(json!({"nested": {"content": [
        {"type": "image", "mimeType": "image/png", "data": "AQIDBA=="},
        {"type": "audio", "mimeType": "audio/wav", "data": "data:audio/wav;base64,AQI="}
    ]}}));
    let rendered = render_tool_output(&out, usize::MAX);
    assert!(!rendered.contains("AQIDBA==") && !rendered.contains("AQI="));
    assert!(rendered.contains("[image/png data: 4 bytes]"));
    assert!(rendered.contains("[audio/wav data: 2 bytes]"));
}

#[test]
fn tier_three_removes_bare_payload_before_truncating() {
    let out = output(json!({"content": [
        {"type": "audio", "mimeType": "audio/wav", "data": "AQID".repeat(2000)}
    ], "note": "padding ".repeat(60)}));
    let raw = serde_json::to_string(&out.output).unwrap();
    assert!(truncate_middle(&raw, 240).contains("AQID"));
    let rendered = render_tool_output(&out, 240);
    assert!(!rendered.contains("AQID"));
    assert!(rendered.chars().count() <= 240 && rendered.contains("[audio/wav data: 6000 bytes]"));
}

#[test]
fn bare_media_blocks_do_not_change_tool_summary_precedence() {
    let mut value = json!({
        "_meta": {"harnx.dev/compaction": "explicit data:image/png;base64,AQID"},
        "content": [
            {"type": "image", "mimeType": "image/png", "data": "AQIDBA=="},
            {"type": "text", "text": "user summary"}
        ]
    });
    assert_eq!(
        render_tool_output(&output(value.clone()), 100),
        "explicit [image/png data: 3 bytes]"
    );
    value.as_object_mut().unwrap().remove("_meta");
    assert_eq!(render_tool_output(&output(value), 100), "user summary");
}
