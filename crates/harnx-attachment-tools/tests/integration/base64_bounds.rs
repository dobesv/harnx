use super::*;

#[tokio::test]
async fn attachment_create_bounds_encoded_length_before_decoding() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let max_bytes: usize = 5 * 1024 * 1024;
    let max_encoded = max_bytes.div_ceil(3) * 4;
    for content in [
        "!".repeat(max_encoded + 1),
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            vec![0; max_bytes + 2],
        ),
    ] {
        let result = ctx
            .invoke(
                "attachment_create",
                json!({
                    "content":content, "mime_type":"image/png", "encoding":"base64"
                }),
                Some(tool_session()),
            )
            .await;
        // Invalid oversized input hits the length guard, not the decoder.
        assert_error_contains(
            &result,
            &format!("base64 content exceeds {max_encoded} encoded bytes"),
        );
        assert!(!first_text(&result).contains("invalid base64"));
    }
}

#[tokio::test]
async fn attachment_create_base64_png_roundtrips_as_image() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let png = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    )
    .expect("decode test PNG");
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);
    let created = ctx
        .invoke(
            "attachment_create",
            json!({"content": encoded, "mime_type": "image/png", "encoding": "base64"}),
            Some(tool_session()),
        )
        .await;
    assert_eq!(created["isError"], false);
    let url = first_text(&created)
        .strip_prefix("Created attachment: ")
        .expect("created attachment URL");
    let read = ctx
        .invoke("attachment_read", json!({"url": url}), None)
        .await;
    assert_eq!(read["isError"], false);
    let image = read["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "image")
        .expect("image block returned");
    assert_eq!(image["mimeType"], "image/png");
    assert_eq!(
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            image["data"].as_str().unwrap()
        )
        .unwrap(),
        png
    );
}

#[tokio::test]
async fn attachment_create_default_utf8_rejects_binary_mime() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let error = assert_tool_error(
        &ctx.toolset,
        ToolInvocation {
            tool: "attachment_create".into(),
            args: json!({"content": "not binary", "mime_type": "image/png"}),
            context: ToolInvocationContext {
                invoking_session: Some(tool_session()),
                ..Default::default()
            },
            cancel: CancellationToken::new(),
        },
    )
    .await;
    assert!(error.contains("only accepts text MIME types"));
}

#[tokio::test]
async fn attachment_create_rejects_invalid_base64_and_unknown_encoding() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let invalid = assert_tool_error(
        &ctx.toolset,
        ToolInvocation {
            tool: "attachment_create".into(),
            args: json!({"content": "%%%", "mime_type": "image/png", "encoding": "base64"}),
            context: ToolInvocationContext {
                invoking_session: Some(tool_session()),
                ..Default::default()
            },
            cancel: CancellationToken::new(),
        },
    )
    .await;
    assert!(invalid.contains("invalid base64 attachment content"));
    let unknown = assert_tool_error(
        &ctx.toolset,
        ToolInvocation {
            tool: "attachment_create".into(),
            args: json!({"content": "x", "mime_type": "text/plain", "encoding": "binary"}),
            context: ToolInvocationContext {
                invoking_session: Some(tool_session()),
                ..Default::default()
            },
            cancel: CancellationToken::new(),
        },
    )
    .await;
    assert!(unknown.contains("unsupported attachment encoding 'binary'"));
}

#[tokio::test]
async fn attachment_create_base64_limit_applies_to_decoded_bytes() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    for (size, succeeds) in [(5 * 1024 * 1024, true), (5 * 1024 * 1024 + 1, false)] {
        let bytes = vec![0u8; size];
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
        let result = ctx.invoke(
            "attachment_create",
            json!({"content": encoded, "mime_type": "application/octet-stream", "encoding": "base64"}),
            Some(tool_session()),
        ).await;
        assert_eq!(result["isError"], !succeeds, "decoded size {size}");
        if !succeeds {
            assert!(first_text(&result).contains("5242881 bytes exceeds the 5 MB limit"));
        }
    }
}
