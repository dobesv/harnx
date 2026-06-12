use harnx_core::message::{ImageUrl, MessageContentPart};
use serde_json::Value;

/// Walk output["content"][] for image blocks, returning each as a data-URI ImageUrl part.
/// Non-image results / missing content → empty Vec.
pub fn extract_image_parts(output: &Value) -> Vec<MessageContentPart> {
    let Some(content) = output.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };

    content
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("image") {
                return None;
            }

            let data = block.get("data").and_then(Value::as_str)?;
            let mime = block
                .get("mimeType")
                .or_else(|| block.get("mime_type"))
                .and_then(Value::as_str)?;

            Some(MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("data:{mime};base64,{data}"),
                },
            })
        })
        .collect()
}

/// Replace heavy base64 `data` of image blocks in output["content"][] with a short
/// placeholder so output.to_string() (session logs, OpenAI tool text) stays small.
/// Text blocks untouched. Mutates in place.
pub fn redact_image_data(output: &mut Value) {
    let Some(content) = output.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };

    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }

        let Some(data) = block.get("data").and_then(Value::as_str) else {
            continue;
        };

        let mime = block
            .get("mimeType")
            .or_else(|| block.get("mime_type"))
            .and_then(Value::as_str)
            .unwrap_or("image");
        let placeholder = format!("<image: {mime}, {} base64 chars>", data.chars().count());

        if let Some(object) = block.as_object_mut() {
            object.insert("data".to_string(), Value::String(placeholder));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_image_parts, redact_image_data};
    use harnx_core::message::MessageContentPart;
    use serde_json::json;

    fn image_urls(parts: Vec<MessageContentPart>) -> Vec<String> {
        parts
            .into_iter()
            .filter_map(|part| match part {
                MessageContentPart::ImageUrl { image_url } => Some(image_url.url),
                MessageContentPart::Text { .. } => None,
            })
            .collect()
    }

    #[test]
    fn extract_image_parts_returns_data_uri_for_camel_case_mime() {
        let data = "aGVsbG8=";
        let output = json!({
            "content": [
                {
                    "type": "text",
                    "text": "caption"
                },
                {
                    "type": "image",
                    "data": data,
                    "mimeType": "image/png"
                }
            ]
        });

        let parts = extract_image_parts(&output);
        assert_eq!(
            image_urls(parts),
            vec![format!("data:image/png;base64,{data}")]
        );
    }

    #[test]
    fn extract_image_parts_accepts_snake_case_mime() {
        let data = "aGVsbG8=";
        let output = json!({
            "content": [
                {
                    "type": "image",
                    "data": data,
                    "mime_type": "image/jpeg"
                }
            ]
        });

        let parts = extract_image_parts(&output);
        assert_eq!(
            image_urls(parts),
            vec![format!("data:image/jpeg;base64,{data}")]
        );
    }

    #[test]
    fn extract_image_parts_returns_empty_for_non_image_or_missing_content() {
        let text_only = json!({
            "content": [
                {
                    "type": "text",
                    "text": "only text"
                }
            ]
        });
        let missing_content = json!({"result": "ok"});

        assert!(extract_image_parts(&text_only).is_empty());
        assert!(extract_image_parts(&missing_content).is_empty());
    }

    #[test]
    fn extract_image_parts_skips_image_blocks_missing_mime() {
        let output = json!({
            "content": [
                {
                    "type": "image",
                    "data": "aGVsbG8="
                }
            ]
        });

        assert!(extract_image_parts(&output).is_empty());
    }

    #[test]
    fn redact_image_data_replaces_payload_and_preserves_text_type_and_mime() {
        let data = "aGVsbG8=";
        let text = "caption";
        let mut output = json!({
            "content": [
                {
                    "type": "text",
                    "text": text
                },
                {
                    "type": "image",
                    "data": data,
                    "mimeType": "image/png"
                }
            ]
        });

        redact_image_data(&mut output);

        let serialized = output.to_string();
        assert!(!serialized.contains(data));
        assert!(serialized.contains(text));

        let image_block = &output["content"][1];
        assert_eq!(image_block["type"], "image");
        assert_eq!(image_block["mimeType"], "image/png");
        assert_eq!(image_block["data"], "<image: image/png, 8 base64 chars>");
    }

    #[test]
    fn extract_then_redact_keeps_extracted_parts_owned() {
        let data = "aGVsbG8=";
        let mut output = json!({
            "content": [
                {
                    "type": "image",
                    "data": data,
                    "mimeType": "image/png"
                }
            ]
        });

        let parts = extract_image_parts(&output);
        redact_image_data(&mut output);
        assert_eq!(
            image_urls(parts),
            vec![format!("data:image/png;base64,{data}")]
        );
        assert_eq!(
            output["content"][0]["data"],
            "<image: image/png, 8 base64 chars>"
        );
    }
}
