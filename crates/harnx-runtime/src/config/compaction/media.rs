//! Remove inline media from the summarizer's JSON view, not from stored tool results.

use serde_json::{Map, Value};

pub(super) fn sanitized_json(output: &Value) -> String {
    serde_json::to_string(&sanitize_value(output)).unwrap_or_default()
}

fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(sanitize_object(object)),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        _ => value.clone(),
    }
}

fn sanitize_object(object: &Map<String, Value>) -> Map<String, Value> {
    let marker = media_data_marker(object);
    object
        .iter()
        .map(|(key, value)| {
            let sanitized = match (key.as_str(), &marker) {
                ("data", Some(marker)) => Value::String(marker.clone()),
                _ => sanitize_value(value),
            };
            (key.clone(), sanitized)
        })
        .collect()
}

fn media_data_marker(object: &Map<String, Value>) -> Option<String> {
    let kind = object.get("type")?.as_str()?;
    if !matches!(kind, "image" | "audio") {
        return None;
    }
    let data = object.get("data")?.as_str()?;
    let bytes = decoded_base64_bytes(data)?;
    let mime = object
        .get("mimeType")
        .or_else(|| object.get("mime_type"))
        .and_then(Value::as_str)
        .unwrap_or(kind);
    Some(format!("[{mime} data: {bytes} bytes]"))
}

fn decoded_base64_bytes(data: &str) -> Option<usize> {
    let mut length = 0;
    for byte in data.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' | b'-' | b'_' => length += 1,
            b'=' | b' ' | b'\t' | b'\r' | b'\n' => {}
            // Already-redacted placeholders and external references stay readable.
            _ => return None,
        }
    }
    Some(length / 4 * 3 + length % 4 * 3 / 4)
}
