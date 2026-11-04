use serde_json::{json, Value};

use crate::cli_map::OutputFormat;

/// A successful transport can still carry an MCP tool-domain error. Only the
/// canonical MCP envelope is interpreted here; raw business JSON stays data.
/// Shared with the runtime executor, which classifies these as ToolError
/// before the command layer sees them (PR3 round-2 review R6): one
/// detector, one text extractor, no drift between layers.
pub fn is_mcp_error(value: &Value) -> bool {
    value.get("content").is_some_and(Value::is_array)
        && value.get("isError").and_then(Value::as_bool) == Some(true)
}

pub fn error_text(value: &Value) -> String {
    let text = content_text(value);
    if !text.trim().is_empty() {
        text
    } else if let Some(error) = value.get("error").and_then(Value::as_str) {
        error.into()
    } else {
        "tool reported an error".into()
    }
}

pub(super) fn output(value: &Value, format: Option<OutputFormat>) -> String {
    let structured = value.get("structuredContent");
    let text = match format {
        Some(OutputFormat::Json) => {
            // Without structuredContent the MCP envelope remains inspectable,
            // but inline media is replaced with markers even in JSON mode.
            structured
                .cloned()
                .unwrap_or_else(|| without_inline_media(value))
                .to_string()
        }
        None if structured.is_some() => structured.unwrap().to_string(),
        None | Some(OutputFormat::Text) => {
            if value.get("content").is_some_and(Value::is_array) {
                let text = content_text(value);
                if text.is_empty() {
                    structured.map(text_value).unwrap_or_default()
                } else {
                    text
                }
            } else {
                text_value(value)
            }
        }
    };
    terminated(text)
}

fn text_value(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn terminated(mut text: String) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn block_text(block: &Value) -> String {
    if block.get("type").and_then(Value::as_str) == Some("text") {
        block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    } else {
        attachment_marker(block).unwrap_or_default()
    }
}

fn append_block_text(text: &mut String, part: &str) {
    if part.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(part);
}

fn content_text(value: &Value) -> String {
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let mut text = String::new();
    for block in blocks {
        let part = block_text(block);
        append_block_text(&mut text, &part);
    }
    text
}

fn attachment_marker(block: &Value) -> Option<String> {
    let resource = match block.get("type").and_then(Value::as_str)? {
        "resource" => block.get("resource")?,
        "image" | "audio" | "resource_link" => block,
        _ => return None,
    };
    let uri = resource
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or("inline");
    let mime = resource
        .get("mimeType")
        .or_else(|| resource.get("mime_type"))
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    Some(format!("[attachment: {uri} {mime}]"))
}

fn without_inline_media(value: &Value) -> Value {
    let mut result = value.clone();
    if let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) {
        for block in blocks {
            if let Some(marker) = attachment_marker(block) {
                *block = json!({"type":"text", "text":marker});
            }
        }
    }
    result
}

#[cfg(test)]
mod tests;
