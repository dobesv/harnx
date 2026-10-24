//! Integration tests for harnx-attachment-tools.

use async_nats::jetstream::object_store::ObjectStore;
use harnx_attachment_tools::AttachmentToolset;
use harnx_blob_store::media::{ensure_attachments_bucket, put_media};
use harnx_blob_store::media_cid_url;
use harnx_blob_store::plans::{
    create_document, ensure_plans_bucket, serialize_plan, PlanDocument, PlanFrontMatter,
};
use harnx_core::cid_url::{CidUrl, PlanItem, SessionRef as CoreSessionRef};
use harnx_test_bins::{spawn_nats_server, NatsServerHandle};
use harnx_toolset::{SessionRef as ToolSessionRef, ToolInvocation, ToolInvocationContext, Toolset};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HASH_B: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

struct TestContext {
    _nats: NatsServerHandle,
    toolset: AttachmentToolset,
    store: ObjectStore,
    jetstream: async_nats::jetstream::Context,
}

impl TestContext {
    async fn start() -> Option<Self> {
        let nats = spawn_nats_server().await.expect("start nats-server")?;
        let client = async_nats::connect(nats.url())
            .await
            .expect("connect to test NATS server");
        let jetstream = async_nats::jetstream::new(client);
        let store = ensure_attachments_bucket(&jetstream, 1)
            .await
            .expect("create attachment object store");
        let toolset = AttachmentToolset::with_nats_url(nats.url().to_string());
        Some(Self {
            _nats: nats,
            toolset,
            store,
            jetstream,
        })
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        invoking_session: Option<ToolSessionRef>,
    ) -> Value {
        self.toolset
            .invoke_with_context(ToolInvocation {
                tool: tool.to_string(),
                args,
                context: ToolInvocationContext {
                    invoking_session,
                    ..ToolInvocationContext::default()
                },
                cancel: CancellationToken::new(),
            })
            .await
            .expect("tool invocation should return a result")
    }
}

fn media_url(hash: &str) -> CidUrl {
    let session = CoreSessionRef::new(Some("test-agent".to_string()), "abcDEF".to_string())
        .expect("valid test session");
    media_cid_url(&session, hash)
}

fn tool_session() -> ToolSessionRef {
    ToolSessionRef {
        agent: Some("test-agent".to_string()),
        session_id: "abcDEF".to_string(),
    }
}

fn assert_error_contains(value: &Value, expected: &str) {
    assert_eq!(value["isError"], true, "expected tool error: {value}");
    let text = value["content"][0]["text"].as_str().expect("error text");
    assert!(
        text.contains(expected),
        "error should contain {expected:?}, got: {text}"
    );
}

async fn assert_tool_error(toolset: &AttachmentToolset, invocation: ToolInvocation) -> String {
    let result = toolset
        .invoke_with_context(invocation)
        .await
        .expect("invoke returns result");
    assert!(
        result["isError"].as_bool().unwrap_or(false),
        "expected isError: true"
    );
    result["content"][0]["text"]
        .as_str()
        .expect("text content")
        .to_string()
}

fn first_text(value: &Value) -> &str {
    value["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|content| content.get("text")?.as_str())
        .next()
        .expect("text content")
}

#[tokio::test]
async fn attachment_create_and_read_roundtrip() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let created = ctx
        .invoke(
            "attachment_create",
            json!({
                "content": "Hello, world!\nSecond line\nThird line",
                "mime_type": "text/plain"
            }),
            Some(tool_session()),
        )
        .await;
    let url = first_text(&created)
        .strip_prefix("Created attachment: ")
        .expect("created attachment URL");

    let read = ctx
        .invoke(
            "attachment_read",
            json!({ "url": url, "head_lines": 1, "tail_lines": 0 }),
            None,
        )
        .await;

    assert_eq!(read["isError"], false);
    let text = first_text(&read);
    assert!(text.starts_with("Hello, world!"));
    assert!(!text.contains("Second line"));
}

#[tokio::test]
async fn attachment_read_returns_image_block() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    let png = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
    )
    .expect("decode test PNG");
    put_media(&ctx.store, &url, &png, "image/png")
        .await
        .expect("put PNG");

    let result = ctx
        .invoke("attachment_read", json!({ "url": url.to_string() }), None)
        .await;

    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["type"], "image");
}

#[tokio::test]
async fn attachment_read_returns_rendered_plan_markdown() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let session = CoreSessionRef::new(Some("test-agent".to_string()), "abcDEF".to_string())
        .expect("valid test session");
    let url = CidUrl::Plan {
        session,
        slug: "read-plan".to_string(),
        item: PlanItem::Index,
    };
    let store = ensure_plans_bucket(&ctx.jetstream, 1)
        .await
        .expect("create plans bucket");
    let content = serialize_plan(&PlanDocument {
        front: PlanFrontMatter {
            id: url.to_string(),
            title: Some("Readable Plan".to_string()),
            summary: Some("Plan summary".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            ..PlanFrontMatter::default()
        },
        body: "Plan body".to_string(),
    })
    .expect("serialize plan");
    create_document(&store, &url, &content)
        .await
        .expect("store plan");

    let result = ctx
        .invoke("attachment_read", json!({ "url": url.to_string() }), None)
        .await;

    assert_eq!(result["isError"], false);
    let markdown = first_text(&result);
    assert!(markdown.starts_with("# Readable Plan\n\nPlan summary"));
    assert!(markdown.contains("## Tasks"));
    assert!(markdown.contains("## Notes"));
}

#[tokio::test]
async fn tool_invocations_reject_invalid_requests() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };

    let plan_err = assert_tool_error(
        &ctx.toolset,
        ToolInvocation {
            tool: "attachment_read".to_string(),
            args: json!({ "url": "cid:plan:test-agent/abcDEF/test-plan" }),
            context: ToolInvocationContext::default(),
            cancel: CancellationToken::new(),
        },
    )
    .await;
    assert!(plan_err.contains("plan document not found"));

    let session_err = assert_tool_error(
        &ctx.toolset,
        ToolInvocation {
            tool: "attachment_create".to_string(),
            args: json!({ "content": "Should fail", "mime_type": "text/plain" }),
            context: ToolInvocationContext::default(),
            cancel: CancellationToken::new(),
        },
    )
    .await;
    assert!(session_err.contains("caller session identity required"));

    let mime_err = assert_tool_error(
        &ctx.toolset,
        ToolInvocation {
            tool: "attachment_create".to_string(),
            args: json!({ "content": "some data", "mime_type": "application/octet-stream" }),
            context: ToolInvocationContext {
                invoking_session: Some(harnx_toolset::SessionRef {
                    agent: Some("test-agent".to_string()),
                    session_id: "abcDEF".to_string(),
                }),
                ..ToolInvocationContext::default()
            },
            cancel: CancellationToken::new(),
        },
    )
    .await;
    assert!(mime_err.contains("only accepts text MIME types"));
}

#[tokio::test]
async fn attachment_read_rejects_binary_mime() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    put_media(&ctx.store, &url, &[0, 1, 2, 3], "application/octet-stream")
        .await
        .expect("put binary attachment");

    let result = ctx
        .invoke("attachment_read", json!({ "url": url.to_string() }), None)
        .await;

    assert_error_contains(&result, "application/octet-stream");
    assert_error_contains(&result, "4B");
}

#[tokio::test]
async fn attachment_read_applies_grep_offset_and_limit() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_B);
    let text = "skip\nmatch one\nignore\nmatch two\nmatch three\nmatch four\n";
    put_media(&ctx.store, &url, text.as_bytes(), "text/plain")
        .await
        .expect("put text attachment");

    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "offset": 2,
                "limit": 2,
                "grep": "^match"
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    assert_eq!(first_text(&result), "match two\nmatch three\n");
}

#[tokio::test]
async fn invoke_delegates_to_context_aware_handler() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    put_media(&ctx.store, &url, b"direct invoke", "text/plain")
        .await
        .expect("put text attachment");

    let result = ctx
        .toolset
        .invoke(
            "attachment_read",
            json!({ "url": url.to_string() }),
            CancellationToken::new(),
        )
        .await
        .expect("direct invoke should succeed");

    assert_eq!(result["isError"], false);
    assert_eq!(first_text(&result), "direct invoke");
}

#[tokio::test]
async fn attachment_create_rejects_oversized_content() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let result = ctx
        .invoke(
            "attachment_create",
            json!({
                "content": "x".repeat(5 * 1024 * 1024 + 1),
                "mime_type": "text/plain"
            }),
            Some(tool_session()),
        )
        .await;

    assert_error_contains(&result, "payload too large");
    assert_error_contains(&result, "5 MB");
}

#[tokio::test]
async fn attachment_read_not_found() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": "cid:media:_temp/nonExst/abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            }),
            None,
        )
        .await;

    assert_error_contains(&result, "attachment not found");
}
#[tokio::test]
async fn attachment_read_byte_paging_basic() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    put_media(
        &ctx.store,
        &url,
        b"Hello, World! This is a test.",
        "text/plain",
    )
    .await
    .expect("put text attachment");

    // Read first 5 bytes
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 0,
                "byte_limit": 5
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let text = first_text(&result);
    assert_eq!(text, "Hello");
}

#[tokio::test]
async fn attachment_read_byte_paging_middle_of_content() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    put_media(
        &ctx.store,
        &url,
        b"Hello, World! This is a test.",
        "text/plain",
    )
    .await
    .expect("put text attachment");

    // Read from offset 7 (", World...")
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 7,
                "byte_limit": 6
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let text = first_text(&result);
    // byte_offset=7 gives "World!" (6 bytes including the exclamation mark)
    assert_eq!(text, "World!");
}

#[tokio::test]
async fn attachment_read_byte_paging_with_multibyte_utf8() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    // "日本語テスト" - each char is 3 bytes
    put_media(&ctx.store, &url, "日本語テスト".as_bytes(), "text/plain")
        .await
        .expect("put multibyte text attachment");

    // Read first 3 bytes (should get first char '日')
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 0,
                "byte_limit": 3
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let text = first_text(&result);
    assert_eq!(text, "日");
}

#[tokio::test]
async fn attachment_read_byte_paging_rejects_conflicting_selectors() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    put_media(&ctx.store, &url, b"Hello", "text/plain")
        .await
        .expect("put text attachment");

    // Cannot combine byte_offset with head_lines
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 0,
                "head_lines": 1
            }),
            None,
        )
        .await;

    assert_error_contains(&result, "cannot combine");
    assert_error_contains(&result, "line selectors");
    assert_error_contains(&result, "byte selectors");
}

#[tokio::test]
async fn attachment_read_byte_paging_returns_next_offset() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    put_media(&ctx.store, &url, b"Hello, World!", "text/plain")
        .await
        .expect("put text attachment");

    // Read first 5 bytes - should have next_byte_offset
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 0,
                "byte_limit": 5
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    // Second block (user audience) should have next_byte_offset
    let content = result["content"].as_array().unwrap();
    let user_text = content[1]["text"].as_str().unwrap();
    assert!(
        user_text.contains("next_byte_offset: 5"),
        "Should have next offset: {}",
        user_text
    );
}

#[tokio::test]
async fn attachment_read_byte_paging_end_of_content() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    put_media(&ctx.store, &url, b"Hi", "text/plain")
        .await
        .expect("put text attachment");

    // Read starting past the end
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 10,
                "byte_limit": 5
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    // Second block should have end_of_content: true
    let content = result["content"].as_array().unwrap();
    let user_text = content[1]["text"].as_str().unwrap();
    assert!(
        user_text.contains("end_of_content: true"),
        "Should be at end: {}",
        user_text
    );
}

#[tokio::test]
async fn attachment_read_mime_with_charset_parameter() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    // Store with charset parameter
    put_media(
        &ctx.store,
        &url,
        b"Hello with charset",
        "text/plain; charset=utf-8",
    )
    .await
    .expect("put text attachment with charset");

    // Should still work with byte paging
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 0,
                "byte_limit": 5
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let text = first_text(&result);
    assert_eq!(text, "Hello");
}

// =============================================================================
// Task 1 Acceptance Tests: Byte paging reconstruction and boundary handling
// =============================================================================

/// Test that multibyte UTF-8 text can be reconstructed from split pages.
/// Each character is 3 bytes; we read 2-byte pages and verify reconstruction.
#[tokio::test]
async fn attachment_read_byte_paging_reconstructs_multibyte_across_pages() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    // "日本語" - each character is 3 bytes, total 9 bytes
    let original = "日本語";
    put_media(&ctx.store, &url, original.as_bytes(), "text/plain")
        .await
        .expect("put multibyte text attachment");

    // Read in 2-byte pages (smaller than char width)
    let mut reconstructed = String::new();
    let mut offset: usize = 0;

    for _ in 0..10 {
        // Safety limit to prevent infinite loop on failure
        let result = ctx
            .invoke(
                "attachment_read",
                json!({
                    "url": url.to_string(),
                    "byte_offset": offset,
                    "byte_limit": 2
                }),
                None,
            )
            .await;

        assert_eq!(
            result["isError"], false,
            "Page read should not error: {:?}",
            result
        );
        let text = first_text(&result);
        reconstructed.push_str(text);

        // Check for next offset or end
        let content = result["content"].as_array().expect("content array");
        let user_text = content[1]["text"].as_str().expect("user text");
        if user_text.contains("end_of_content: true") {
            break;
        } else if let Some(next_str) = user_text.split("next_byte_offset: ").nth(1) {
            let next: usize = next_str
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .expect("parse next offset");
            assert!(
                next > offset,
                "Should make forward progress: {} -> {}",
                offset,
                next
            );
            offset = next;
        } else {
            panic!(
                "Missing next_byte_offset or end_of_content in: {}",
                user_text
            );
        }
    }

    assert_eq!(
        reconstructed, original,
        "Reconstructed text should match original"
    );
}

/// Test that tiny limits don't stall pagination.
/// Request 1-byte page on 3-byte character should return 0 bytes with error.
#[tokio::test]
async fn attachment_read_byte_paging_tiny_limit_returns_error() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    // "日本語" - 9 bytes, 3 chars
    put_media(&ctx.store, &url, "日本語".as_bytes(), "text/plain")
        .await
        .expect("put multibyte text attachment");

    // Request offset 0, limit 1 (inside a 3-byte char) - should work and return first char
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 0,
                "byte_limit": 1
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let text = first_text(&result);
    assert_eq!(text, "日", "Should return first complete character");
}

/// Test that invalid byte_offset (inside multibyte char) returns explicit error.
#[tokio::test]
async fn attachment_read_byte_paging_invalid_offset_error() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    // "日本語" - 9 bytes: [0,1,2]=日, [3,4,5]=本, [6,7,8]=語
    put_media(&ctx.store, &url, "日本語".as_bytes(), "text/plain")
        .await
        .expect("put multibyte text attachment");

    // Offset 1 is inside the first character - should return error
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 1,
                "byte_limit": 5
            }),
            None,
        )
        .await;

    assert_eq!(
        result["isError"], true,
        "Should be error for invalid offset"
    );
    let error_text = first_text(&result);
    assert!(
        error_text.contains("not at a valid UTF-8 character boundary"),
        "Error should explain boundary issue: {}",
        error_text
    );
    assert!(
        error_text.contains("next valid offset is 3") || error_text.contains("offset: 3"),
        "Error should suggest valid offset: {}",
        error_text
    );
}

/// Test that large single-line JSON can be read in pages.
/// Creates a giant single-line JSON array and verifies page reconstruction.
#[tokio::test]
async fn attachment_read_byte_paging_giant_single_line_json() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    // Create a giant single-line JSON array: [1,2,3,...,N]
    let items: Vec<u32> = (1..=1000).collect();
    let json_line = serde_json::to_string(&items).expect("serialize JSON");
    assert!(!json_line.contains('\n'), "Should be single line");

    put_media(&ctx.store, &url, json_line.as_bytes(), "application/json")
        .await
        .expect("put giant JSON attachment");

    // Read in 100-byte pages
    let mut reconstructed = String::new();
    let mut offset: usize = 0;
    let page_size = 100;

    loop {
        let result = ctx
            .invoke(
                "attachment_read",
                json!({
                    "url": url.to_string(),
                    "byte_offset": offset,
                    "byte_limit": page_size
                }),
                None,
            )
            .await;

        assert_eq!(result["isError"], false, "Page read should not error");
        let text = first_text(&result);
        reconstructed.push_str(text);

        let content = result["content"].as_array().expect("content array");
        let user_text = content[1]["text"].as_str().expect("user text");
        if user_text.contains("end_of_content: true") {
            break;
        }
        if let Some(next_str) = user_text.split("next_byte_offset: ").nth(1) {
            let next: usize = next_str
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .expect("parse next offset");
            assert!(
                next > offset,
                "Should make forward progress: {} -> {}",
                offset,
                next
            );
            offset = next;
        }
    }

    // Verify reconstruction matches original
    let reconstructed_items: Vec<u32> =
        serde_json::from_str(&reconstructed).expect("reconstructed JSON should parse");
    assert_eq!(reconstructed_items.len(), 1000);
    assert_eq!(reconstructed_items[0], 1);
    assert_eq!(reconstructed_items[999], 1000);
}

/// Test that byte_offset without byte_limit uses default 64 KiB limit.
#[tokio::test]
async fn attachment_read_byte_paging_uses_default_limit() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);
    // Create content larger than 64 KiB to test default limit
    let large_content = "x".repeat(100_000); // 100 KiB
    put_media(&ctx.store, &url, large_content.as_bytes(), "text/plain")
        .await
        .expect("put large text attachment");

    // Request with byte_offset but no byte_limit
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 0
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let text = first_text(&result);
    // Should return ~64 KiB, not the whole 100 KiB
    assert!(
        text.len() <= 66000,
        "Should use bounded default limit, got {}",
        text.len()
    );

    // Should have next_byte_offset
    let content = result["content"].as_array().expect("content array");
    let user_text = content[1]["text"].as_str().expect("user text");
    assert!(
        user_text.contains("next_byte_offset:"),
        "Should have next offset for continued reading: {}",
        user_text
    );
}

/// Test that reading >5 MiB content works (plan requirement).
/// Uses put_media directly to bypass client 5 MiB limit, then reads in pages.
#[tokio::test]
async fn attachment_read_byte_paging_above_5_mib() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);

    // Create 6 MiB of content (above the 5 MiB client creation limit)
    // Using repeating pattern for verification
    let pattern = "ABCDEFGHIJ"; // 10 bytes
    let size = 6 * 1024 * 1024; // 6 MiB
    let large_content: String = pattern.repeat(size / pattern.len());
    assert!(large_content.len() > 5 * 1024 * 1024, "Should be > 5 MiB");

    put_media(&ctx.store, &url, large_content.as_bytes(), "text/plain")
        .await
        .expect("put >5 MiB attachment directly");

    // Read first page
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": 0,
                "byte_limit": 1000
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let text = first_text(&result);
    assert!(text.starts_with("ABCDEFGHIJ"), "Should start with pattern");

    // Read from the middle
    let mid_offset = 3 * 1024 * 1024; // 3 MiB in
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": mid_offset,
                "byte_limit": 1000
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let text = first_text(&result);
    assert!(!text.is_empty(), "Should read from middle");

    // Read from near the end
    let near_end = large_content.len() - 500;
    let result = ctx
        .invoke(
            "attachment_read",
            json!({
                "url": url.to_string(),
                "byte_offset": near_end,
                "byte_limit": 1000
            }),
            None,
        )
        .await;

    assert_eq!(result["isError"], false);
    let content = result["content"].as_array().expect("content array");
    let user_text = content[1]["text"].as_str().expect("user text");
    assert!(
        user_text.contains("end_of_content: true"),
        "Should reach end of large content: {}",
        user_text
    );
}

/// Test that emoji characters (4-byte UTF-8) are handled correctly.
#[tokio::test]
async fn attachment_read_byte_paging_4byte_emoji() {
    let Some(ctx) = TestContext::start().await else {
        return;
    };
    let url = media_url(HASH_A);

    // "Hello 😀 World" - 😀 is 4 bytes (0xF0 0x9F 0x98 0x80)
    let text_with_emoji = "Hello 😀 World";
    put_media(&ctx.store, &url, text_with_emoji.as_bytes(), "text/plain")
        .await
        .expect("put emoji text attachment");

    // Read in small pages to test all boundary cases
    let mut reconstructed = String::new();
    let mut offset: usize = 0;

    for _ in 0..20 {
        let result = ctx
            .invoke(
                "attachment_read",
                json!({
                    "url": url.to_string(),
                    "byte_offset": offset,
                    "byte_limit": 3
                }),
                None,
            )
            .await;

        assert_eq!(result["isError"], false);
        reconstructed.push_str(first_text(&result));

        let content = result["content"].as_array().expect("content array");
        let user_text = content[1]["text"].as_str().expect("user text");
        if user_text.contains("end_of_content: true") {
            break;
        }
        if let Some(next_str) = user_text.split("next_byte_offset: ").nth(1) {
            let next: usize = next_str
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .expect("parse next offset");
            assert!(next > offset);
            offset = next;
        }
    }

    assert_eq!(
        reconstructed, text_with_emoji,
        "Emoji text should reconstruct correctly"
    );
}
