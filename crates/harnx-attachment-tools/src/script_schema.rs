//! The fields of the attachment tools' script-style results, for
//! `help attachment_*`, and the `ScriptOutput` mappings that route them.

use harnx_toolset::{output_field, output_nullable_field, ScriptOutput, ToolSpec};
use serde_json::{json, Value};

fn list_of(kind: &str, description: &str) -> Value {
    json!({ "type": "array", "items": { "type": kind }, "description": description })
}

/// `attachment_read` prints the text on stdout and the notes on stderr.
pub(crate) fn read_script_output() -> ScriptOutput {
    ScriptOutput {
        stdout: Some("text".to_string()),
        stderr: Some("notes".to_string()),
        summary: Some("the attachment text; paging and cut-off notes on stderr".to_string()),
        ..Default::default()
    }
}

/// `attachment_create` prints its confirmation line on stdout.
pub(crate) fn create_script_output() -> ScriptOutput {
    ScriptOutput {
        stdout: Some("message".to_string()),
        summary: Some("the created attachment's cid: URL and MIME type".to_string()),
        ..Default::default()
    }
}

pub(crate) fn read_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "text": output_field("string", "the selected attachment text, without truncation markers"),
            "notes": list_of("string", "what was cut off and how to continue"),
            "url": output_field("string", "the cid: URL that was read"),
            "content_type": output_field("string", "the attachment's MIME type"),
            "byte_offset": output_field("integer", "byte mode: zero-based offset this page starts at"),
            "next_byte_offset": output_nullable_field(
                "integer",
                "byte mode: offset to pass as byte_offset next; null at the end of content"
            ),
            "end_of_content": output_field(
                "boolean",
                "byte mode: true when this page reached the end of the content"
            ),
            "total_bytes": output_field("integer", "byte mode: size of the whole attachment in bytes"),
        }
    })
}

pub(crate) fn create_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": output_field("string", "the newline-terminated confirmation line with the cid: URL"),
            "url": output_field("string", "the created attachment's cid: URL"),
            "mime_type": output_field("string", "the stored MIME type"),
        }
    })
}

/// `attachment_read` with its shell form and result fields.
pub(crate) fn read_tool(spec: ToolSpec) -> ToolSpec {
    spec.with_script_output(read_script_output())
        .with_output_schema(read_output_schema())
}

/// `attachment_create` with its shell form and result fields.
pub(crate) fn create_tool(spec: ToolSpec) -> ToolSpec {
    spec.with_script_output(create_script_output())
        .with_output_schema(create_output_schema())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script_result::{script_byte_page, script_create_result, script_line_page};
    use crate::AttachmentToolset;
    use harnx_toolset::Toolset;
    use serde_json::json;

    fn spec(name: &str) -> harnx_toolset::ToolSpec {
        AttachmentToolset::new()
            .tools()
            .into_iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("{name} listed"))
    }

    fn declared(name: &str) -> serde_json::Map<String, Value> {
        spec(name)
            .output_schema()
            .expect("output schema declared")
            .get("properties")
            .expect("schema properties")
            .as_object()
            .expect("properties object")
            .clone()
    }

    fn emitted_keys(result: rmcp::model::CallToolResult) -> Vec<String> {
        let value = serde_json::to_value(result).expect("serialize result");
        value
            .get("structuredContent")
            .expect("structured content")
            .as_object()
            .expect("structured object")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn specs_carry_the_script_mapping_and_schema() {
        for name in ["attachment_read", "attachment_create"] {
            let tool = spec(name);
            let output = tool
                .script_output()
                .unwrap_or_else(|| panic!("{name} declares no script output"));
            assert!(output.stdout.is_some(), "{name} maps no stdout field");
            assert!(
                tool.output_schema().is_some(),
                "{name} declares no output schema"
            );
            for field in [&output.stdout, &output.stderr].into_iter().flatten() {
                assert!(declared(name).contains_key(field), "{name} lacks {field}");
            }
        }
    }

    /// Every field the script results emit is declared in the output schema,
    /// in both paging modes, with and without cut-offs.
    #[test]
    fn script_result_fields_match_the_declared_schema() {
        let read = declared("attachment_read");
        let create = declared("attachment_create");
        let text = "alpha\nbêta\n".repeat(64);
        let url = "cid:media:_temp/ab/0000";
        for (params, expected_notes) in [
            (json!({ "url": url }), json!([])),
            (
                json!({ "url": url, "head_lines": 1, "tail_lines": 1 }),
                json!(["126 lines left out"]),
            ),
        ] {
            let params: crate::handlers::AttachmentReadParams =
                serde_json::from_value(params).unwrap();
            let result = script_line_page(&text, &params, "text/plain", "summary".into());
            let value = serde_json::to_value(&result).expect("serialize");
            assert_eq!(
                value.pointer("/structuredContent/notes").unwrap(),
                &expected_notes
            );
            for key in emitted_keys(result) {
                assert!(read.contains_key(&key), "undeclared {key}");
            }
        }
        // Byte mode, mid-content and at the end.
        for byte_limit in [1024, text.len() * 4] {
            let params: crate::handlers::AttachmentReadParams =
                serde_json::from_value(json!({ "url": url, "byte_limit": byte_limit })).unwrap();
            let page = crate::handlers::BytePage::new(text.as_bytes(), &params).unwrap();
            let result = script_byte_page(
                &page,
                &text[page.start..page.end],
                url,
                "text/plain; charset=utf-8",
            );
            for key in emitted_keys(result) {
                assert!(read.contains_key(&key), "undeclared {key}");
            }
        }
        for key in emitted_keys(script_create_result(url, "text/plain")) {
            assert!(create.contains_key(&key), "undeclared {key}");
        }
    }

    #[test]
    fn line_mode_keeps_text_clean_and_notes_on_the_side() {
        let text = "one\ntwo\nthree\nfour\n";
        let params: crate::handlers::AttachmentReadParams = serde_json::from_value(json!({
            "url": "cid:media:_temp/ab/0000",
            "head_lines": 1,
            "tail_lines": 1
        }))
        .unwrap();
        let result = script_line_page(text, &params, "text/plain", "summary".into());
        let value = serde_json::to_value(&result).expect("serialize");
        let structured = value.pointer("/structuredContent").expect("structured");
        assert_eq!(structured["text"], "one\nfour\n");
        assert_eq!(
            structured["notes"],
            json!(["2 lines left out"]),
            "notes carry the omission"
        );
    }
}
