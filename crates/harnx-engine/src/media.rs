use anyhow::{Context, Result};
use harnx_core::cid_url::CidUrl;
use harnx_core::message::{ImageUrl, MessageContentPart};
use serde_json::{json, Value};

/// Walk output["content"][] for image blocks, returning each as an ImageUrl part.
///
/// Recognizes:
/// - Inline `type="image"` with `data` and `mimeType` → data URI
/// - Canonical `type="resource_link"` with `uri` starting with `cid:media:` → cid URL
///
/// Non-image results / missing content → empty Vec. Malformed media image
/// references are errors so neither live execution nor replay silently loses images.
pub fn extract_image_parts(output: &Value) -> Result<Vec<MessageContentPart>> {
    let Some(content) = output.get("content").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut images = Vec::new();
    for block in content {
        let image = match block.get("type").and_then(Value::as_str) {
            Some("image") => extract_inline_image(block),
            Some("resource_link") => extract_resource_link_image(block)?,
            _ => None,
        };
        if let Some(image) = image {
            images.push(image);
        }
    }
    Ok(images)
}

/// Project tool-controlled image syntax failures as per-call recoverable outcomes.
/// Storage and digest checks happen later and must still fail the turn.
pub(crate) fn prepare_tool_result_images(output: &mut Value) -> Vec<MessageContentPart> {
    match extract_image_parts(output) {
        Ok(images) => images,
        Err(error) => {
            // Keep the exact reply for diagnostics; replay applies the same projection
            // to the saved provider outcome without repeating tool side effects.
            *output = json!({
                "is_error": true,
                "error": format!("{error:#}"),
                "original_output": output.take(),
            });
            Vec::new()
        }
    }
}

fn extract_inline_image(block: &Value) -> Option<MessageContentPart> {
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
}

fn extract_resource_link_image(block: &Value) -> Result<Option<MessageContentPart>> {
    let Some(uri) = block.get("uri").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(mime) = block.get("mimeType").and_then(Value::as_str) else {
        return Ok(None);
    };
    let base_mime = mime.split(';').next().unwrap_or("").trim();
    // Only attachment-backed images belong in provider image parts. Other links
    // stay in tool text; in particular, never fetch arbitrary network URLs.
    if !base_mime.to_ascii_lowercase().starts_with("image/") || !uri.starts_with("cid:media:") {
        return Ok(None);
    }
    CidUrl::parse(uri).with_context(|| format!("invalid tool image attachment URL '{uri}'"))?;
    Ok(Some(MessageContentPart::ImageUrl {
        image_url: ImageUrl {
            url: uri.to_string(),
        },
    }))
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

        let parts = extract_image_parts(&output).unwrap();
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

        let parts = extract_image_parts(&output).unwrap();
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

        assert!(extract_image_parts(&text_only).unwrap().is_empty());
        assert!(extract_image_parts(&missing_content).unwrap().is_empty());
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

        assert!(extract_image_parts(&output).unwrap().is_empty());
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

        let parts = extract_image_parts(&output).unwrap();
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

    #[test]
    fn extract_image_parts_recognizes_cid_media_resource_link() {
        let cid = "cid:media:pantheon%2Fatlas/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let output = json!({
            "content": [
                {
                    "type": "text",
                    "text": "caption"
                },
                {
                    "type": "resource_link",
                    "uri": cid,
                    "mimeType": "image/png",
                    "name": "image attachment",
                    "size": 12345
                }
            ]
        });

        let parts = extract_image_parts(&output).unwrap();
        assert_eq!(image_urls(parts), vec![cid.to_string()]);
    }

    #[test]
    fn extract_image_parts_ignores_non_media_cid_resource_link() {
        // cid:plan: URLs are not images
        let output = json!({
            "content": [
                {
                    "type": "resource_link",
                    "uri": "cid:plan:pantheon%2Fatlas/armDRA/my-plan",
                    "mimeType": "text/markdown",
                    "name": "plan link"
                }
            ]
        });

        assert!(extract_image_parts(&output).unwrap().is_empty());
    }

    #[test]
    fn extract_image_parts_ignores_resource_link_without_mime() {
        let output = json!({
            "content": [
                {
                    "type": "resource_link",
                    "uri": "cid:media:pantheon%2Fatlas/armDRA/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "name": "image attachment"
                }
            ]
        });

        assert!(extract_image_parts(&output).unwrap().is_empty());
    }

    #[test]
    fn extract_image_parts_ignores_resource_link_with_external_uri() {
        // External URLs rejected (security)
        let output = json!({
            "content": [
                {
                    "type": "resource_link",
                    "uri": "https://example.com/image.png",
                    "mimeType": "image/png"
                }
            ]
        });

        assert!(extract_image_parts(&output).unwrap().is_empty());
    }

    #[test]
    fn extract_image_parts_rejects_malformed_media_image_links() {
        for uri in [
            "cid:media:missing-owner",
            "cid:media:_temp/abcDEF/not-a-sha256",
            "cid:media:_temp/../0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ] {
            let output = json!({"content": [{
                "type": "resource_link", "uri": uri, "mimeType": "image/png"
            }]});
            let error = extract_image_parts(&output).unwrap_err();
            assert!(error
                .to_string()
                .contains("invalid tool image attachment URL"));
        }
    }

    #[test]
    fn extract_image_parts_does_not_treat_non_image_media_as_images() {
        let output = json!({"content": [{
            "type": "resource_link",
            "uri": "cid:media:_temp/abcDEF/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "mimeType": "audio/wav"
        }]});
        assert!(extract_image_parts(&output).unwrap().is_empty());
    }

    #[test]
    fn extract_image_parts_mixed_inline_and_resource_link() {
        let data = "aGVsbG8=";
        let cid = "cid:media:_temp/abcDEF/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let output = json!({
            "content": [
                {
                    "type": "image",
                    "data": data,
                    "mimeType": "image/png"
                },
                {
                    "type": "text",
                    "text": "separator"
                },
                {
                    "type": "resource_link",
                    "uri": cid,
                    "mimeType": "image/jpeg",
                    "name": "offloaded image",
                    "size": 50000
                }
            ]
        });

        let parts = extract_image_parts(&output).unwrap();
        assert_eq!(
            image_urls(parts),
            vec![format!("data:image/png;base64,{data}"), cid.to_string()]
        );
    }
}
