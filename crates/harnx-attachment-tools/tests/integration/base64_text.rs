use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};

#[tokio::test]
async fn attachment_create_base64_rejects_non_utf8_text() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    // Latin-1 is valid binary data, but attachment_read cannot decode it as text.
    for mime in [
        "text/plain",
        "TEXT/CSV; charset=iso-8859-1",
        "application/json; charset=utf-8",
        "application/xml",
        "application/yaml",
        "application/x-yaml",
    ] {
        let result = ctx
            .invoke(
                "attachment_create",
                json!({"content": STANDARD.encode(b"caf\xe9"), "mime_type": mime, "encoding": "base64"}),
                Some(tool_session()),
            )
            .await;
        assert_error_contains(&result, "text MIME types require UTF-8 content");
    }
}

#[tokio::test]
async fn attachment_create_base64_preserves_non_utf8_binary() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let bytes = b"caf\xe9\x00\xff";
    let result = ctx
        .invoke(
            "attachment_create",
            json!({"content": STANDARD.encode(bytes), "mime_type": "application/octet-stream", "encoding": "base64"}),
            Some(tool_session()),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    let url = CidUrl::parse(
        first_text(&result)
            .strip_prefix("Created attachment: ")
            .unwrap(),
    )
    .unwrap();
    let stored = harnx_blob_store::resolve(&ctx.jetstream, &url)
        .await
        .unwrap();
    assert_eq!(stored.bytes, bytes);
    assert_eq!(stored.mime_type, "application/octet-stream");
}

#[tokio::test]
async fn attachment_create_same_bytes_have_same_cid_across_encodings() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let text = "café 🍳\nsecond line";
    let mut cids = Vec::new();
    for (encoding, content) in [
        ("utf-8", text.to_owned()),
        ("base64", STANDARD.encode(text)),
    ] {
        let result = ctx
            .invoke(
                "attachment_create",
                json!({"content": content, "mime_type": "text/plain", "encoding": encoding}),
                Some(tool_session()),
            )
            .await;
        assert_eq!(result["isError"], false, "{result}");
        cids.push(
            first_text(&result)
                .strip_prefix("Created attachment: ")
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(cids[0], cids[1]);
    let read = ctx
        .invoke("attachment_read", json!({"url": cids[1]}), None)
        .await;
    assert_eq!(read["isError"], false, "{read}");
    assert!(first_text(&read).contains("café 🍳"));
}
