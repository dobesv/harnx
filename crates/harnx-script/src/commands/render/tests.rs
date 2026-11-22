use serde_json::{json, Value};

use super::{error_text, is_mcp_error, output};
use crate::cli_map::OutputFormat::{Json, Text};

#[test]
fn rendering_modes_table() {
    for (value, format, expected) in [
        (
            json!({"content":[{"type":"text","text":"fallback"}],"structuredContent":{"answer":42}}),
            None,
            "{\"answer\":42}\n",
        ),
        (
            json!({"content":[{"type":"text","text":"fallback"}],"structuredContent":{"answer":42}}),
            Some(Text),
            "fallback\n",
        ),
        (
            json!({"content":[],"structuredContent":null}),
            None,
            "null\n",
        ),
        (
            json!({"content":[],"structuredContent":"structured text"}),
            Some(Text),
            "structured text\n",
        ),
        (
            json!({"content":[{"type":"text","text":"first\n"},{"type":"text","text":"second"}]}),
            None,
            "first\nsecond\n",
        ),
        (json!({"content":[]}), None, ""),
        (json!("raw text"), None, "raw text\n"),
        (json!("raw text"), Some(Json), "\"raw text\"\n"),
        (json!([1, 2]), None, "[1,2]\n"),
        (json!(false), Some(Text), "false\n"),
        (json!(null), None, "null\n"),
        (json!({"answer":42}), Some(Text), "{\"answer\":42}\n"),
        (
            json!({"content":[{"type":"text","text":"json text"}]}),
            Some(Json),
            "{\"content\":[{\"type\":\"text\",\"text\":\"json text\"}]}\n",
        ),
    ] {
        assert_eq!(output(&value, format), expected, "{value}, {format:?}");
    }
}

#[test]
fn media_blocks_become_attachment_markers_without_binary_data() {
    let value = json!({"content":[
        {"type":"text","text":"before"},
        {"type":"image","data":"SECRET-BASE64","mimeType":"image/png"},
        {"type":"resource_link","uri":"cid:media:agent/session/abc","mimeType":"image/jpeg"},
        {"type":"resource","resource":{"uri":"cid:media:agent/session/def","mimeType":"application/pdf","blob":"SECRET-BLOB"}},
        {"type":"audio","data":"SECRET-AUDIO","mimeType":"audio/wav"},
        {"type":"resource_link","uri":"https://example.com/resource"}
    ]});
    let expected = concat!(
        "before\n[attachment: inline image/png]\n",
        "[attachment: cid:media:agent/session/abc image/jpeg]\n",
        "[attachment: cid:media:agent/session/def application/pdf]\n",
        "[attachment: inline audio/wav]\n",
        "[attachment: https://example.com/resource application/octet-stream]\n"
    );
    assert_eq!(output(&value, None), expected);
    assert_eq!(output(&value, Some(Text)), expected);
    let rendered = output(&value, Some(Json));
    let document: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(document["content"][1]["type"], "text");
    assert_eq!(
        document["content"][1]["text"],
        "[attachment: inline image/png]"
    );
    assert!(!rendered.contains("SECRET"));
    assert!(value["content"][1].get("data").is_some(), "input unchanged");
}

#[test]
fn mcp_domain_errors_are_distinct_from_raw_status_like_data() {
    for value in [
        json!({"isError":true}),
        json!({"is_error":true,"error":"business data"}),
        json!({"blocked_by_hook":true}),
        json!({"content":[],"isError":false}),
        json!({"content":[{"type":"text","text":"boom"}]}),
        json!({"content":[{"type":"text","text":"ok"}],"isError":false}),
    ] {
        assert!(!is_mcp_error(&value), "{value}");
    }
    let domain = json!({
        "content": [
            {"type":"text","text":"file missing"},
            {"type":"text","text":"and this too"},
        ],
        "isError": true,
    });
    assert!(is_mcp_error(&domain));
    assert_eq!(error_text(&domain), "file missing\nand this too");
    assert_eq!(
        error_text(&json!({"content":[],"isError":true})),
        "tool reported an error"
    );
    assert_eq!(
        error_text(&json!({"content":[],"isError":true,"error":"detail"})),
        "detail"
    );
}

fn audience_result() -> Value {
    json!({"content":[
        {"type":"text","text":"a.rs\nb.rs","annotations":{"audience":["assistant"]}},
        {"type":"text","text":"Found 2 matches","annotations":{"audience":["user"]}},
        {"type":"text","text":"both","annotations":{"audience":["user","assistant"]}},
        {"type":"text","text":"plain"},
        {"type":"text","text":"priority only","annotations":{"priority":1.0}}
    ]})
}

#[test]
fn text_output_skips_blocks_not_addressed_to_the_assistant() {
    let expected = "a.rs\nb.rs\nboth\nplain\npriority only\n";
    assert_eq!(output(&audience_result(), None), expected);
    assert_eq!(output(&audience_result(), Some(Text)), expected);
    // The envelope is the escape hatch and keeps every block.
    assert!(output(&audience_result(), Some(Json)).contains("Found 2 matches"));
}

#[test]
fn user_only_success_prints_nothing() {
    let value = json!({"content":[
        {"type":"text","text":"Wrote 3 files","annotations":{"audience":["user"]}}
    ]});
    assert_eq!(output(&value, None), "");
}

#[test]
fn empty_audience_array_is_not_addressed_to_the_assistant() {
    let value = json!({"content":[
        {"type":"text","text":"nobody","annotations":{"audience":[]}},
        {"type":"text","text":"kept"}
    ]});
    assert_eq!(output(&value, None), "kept\n");
}

#[test]
fn error_text_falls_back_to_user_blocks_when_nothing_else_is_left() {
    let user_only = json!({"isError":true,"content":[
        {"type":"text","text":"denied","annotations":{"audience":["user"]}}
    ]});
    assert_eq!(error_text(&user_only), "denied");
    let mixed = json!({"isError":true,"content":[
        {"type":"text","text":"path not found","annotations":{"audience":["assistant"]}},
        {"type":"text","text":"Read failed","annotations":{"audience":["user"]}}
    ]});
    assert_eq!(error_text(&mixed), "path not found");
    let blank_assistant = json!({"isError":true,"content":[
        {"type":"text","text":"  \n","annotations":{"audience":["assistant"]}},
        {"type":"text","text":"denied","annotations":{"audience":["user"]}}
    ]});
    assert_eq!(error_text(&blank_assistant), "denied");
}
