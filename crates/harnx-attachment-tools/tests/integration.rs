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

#[path = "integration/paging_fixture.rs"]
mod paging_fixture;
use paging_fixture::{assert_page, metadata, StoredAttachment};

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
    let Some(stored) =
        StoredAttachment::start(b"Hello, World! This is a test.", "text/plain").await
    else {
        return;
    };
    assert_page(&stored.page(0, 5).await, "Hello", Some(5));
}

#[tokio::test]
async fn attachment_read_byte_paging_middle_of_content() {
    let Some(stored) =
        StoredAttachment::start(b"Hello, World! This is a test.", "text/plain").await
    else {
        return;
    };
    assert_page(&stored.page(7, 6).await, "World!", Some(13));
}

#[tokio::test]
async fn attachment_read_byte_paging_with_multibyte_utf8() {
    let Some(stored) = StoredAttachment::start("日本語テスト".as_bytes(), "text/plain").await
    else {
        return;
    };
    assert_page(&stored.page(0, 3).await, "日", Some(3));
}

#[tokio::test]
async fn attachment_read_byte_paging_rejects_conflicting_selectors() {
    let Some(stored) = StoredAttachment::start(b"Hello", "text/plain").await else {
        return;
    };
    for selector in ["offset", "limit", "head_lines", "tail_lines", "grep"] {
        let mut args = json!({ "byte_offset": 0 });
        args[selector] = if selector == "grep" {
            json!("Hello")
        } else {
            json!(1)
        };
        let result = stored.read(args).await;
        for expected in ["cannot combine", "line selectors", "byte selectors"] {
            assert_error_contains(&result, expected);
        }
    }
}

#[tokio::test]
async fn attachment_read_byte_paging_returns_next_offset() {
    let Some(stored) = StoredAttachment::start(b"Hello, World!", "text/plain").await else {
        return;
    };
    assert_page(&stored.page(0, 5).await, "Hello", Some(5));
}

#[tokio::test]
async fn attachment_read_byte_paging_end_of_content() {
    let Some(stored) = StoredAttachment::start(b"Hi", "text/plain").await else {
        return;
    };
    assert_page(&stored.page(10, 5).await, "", None);
}

#[tokio::test]
async fn attachment_read_mime_with_charset_parameter() {
    let Some(stored) =
        StoredAttachment::start(b"Hello with charset", "text/plain; charset=utf-8").await
    else {
        return;
    };
    assert_page(&stored.page(0, 5).await, "Hello", Some(5));
}

#[tokio::test]
async fn attachment_read_byte_paging_reconstructs_multibyte_across_pages() {
    let original = "日本語";
    let Some(stored) = StoredAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    assert_eq!(stored.reconstruct(2).await, original);
}

#[tokio::test]
async fn attachment_read_byte_paging_tiny_limit_returns_complete_character() {
    let Some(stored) = StoredAttachment::start("日本語".as_bytes(), "text/plain").await else {
        return;
    };
    assert_page(&stored.page(0, 1).await, "日", Some(3));
}

#[tokio::test]
async fn attachment_read_byte_paging_invalid_offset_error() {
    let Some(stored) = StoredAttachment::start("日本語".as_bytes(), "text/plain").await else {
        return;
    };
    let result = stored.page(1, 5).await;
    assert_error_contains(&result, "not at a valid UTF-8 character boundary");
    assert_error_contains(&result, "next valid offset is 3");
}

#[tokio::test]
async fn attachment_read_byte_paging_giant_single_line_json() {
    let items: Vec<u32> = (1..=1000).collect();
    let original = serde_json::to_string(&items).expect("serialize JSON");
    assert!(!original.contains('\n'));
    let Some(stored) = StoredAttachment::start(original.as_bytes(), "application/json").await
    else {
        return;
    };
    let reconstructed = stored.reconstruct(100).await;
    assert_eq!(reconstructed, original);
    assert_eq!(
        serde_json::from_str::<Vec<u32>>(&reconstructed).unwrap(),
        items
    );
}

#[tokio::test]
async fn attachment_read_byte_paging_uses_default_limit() {
    let original = "x".repeat(100_000);
    let Some(stored) = StoredAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    let result = stored.read(json!({ "byte_offset": 0 })).await;
    assert_page(&result, &original[..65_536], Some(65_536));
}

#[tokio::test]
async fn attachment_read_byte_paging_above_5_mib() {
    let original = "ABCDEFGHIJ".repeat(6 * 1024 * 1024 / 10);
    assert!(original.len() > 5 * 1024 * 1024);
    let Some(stored) = StoredAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    for offset in [0, 3 * 1024 * 1024, original.len() - 500] {
        let end = (offset + 1000).min(original.len());
        let next = (end < original.len()).then_some(end);
        assert_page(
            &stored.page(offset, 1000).await,
            &original[offset..end],
            next,
        );
    }
}

#[tokio::test]
async fn attachment_read_byte_paging_4byte_emoji() {
    let original = "Hello 😀 World";
    let Some(stored) = StoredAttachment::start(original.as_bytes(), "text/plain").await else {
        return;
    };
    assert_eq!(stored.reconstruct(3).await, original);
}

#[tokio::test]
async fn attachment_read_malformed_utf8_with_tiny_limit_returns_error() {
    let Some(stored) = StoredAttachment::start(&[0xE0, 0x80, 0x00], "text/plain").await else {
        return;
    };
    for limit in [1, 2, 3] {
        assert_error_contains(&stored.page(0, limit).await, "invalid UTF-8");
    }
}

#[tokio::test]
async fn attachment_read_yaml_mime_types_and_parameters() {
    for mime in [
        "application/x-yaml",
        "application/x-yaml; charset=utf-8",
        "application/yaml; charset=utf-8",
        "Application/X-Yaml; charset=UTF-8",
    ] {
        assert_yaml_reads(mime).await;
    }
}

async fn assert_yaml_reads(mime: &str) {
    let original = "name: 日本語\ncount: 2";
    let Some(stored) = StoredAttachment::start(original.as_bytes(), mime).await else {
        return;
    };
    assert_eq!(stored.reconstruct(2).await, original, "{mime}");
    let lines = stored
        .read(json!({ "head_lines": 1, "tail_lines": 0 }))
        .await;
    assert_eq!(lines["isError"], false, "{mime}: {lines}");
    assert!(first_text(&lines).starts_with("name: 日本語"));
    assert!(!first_text(&lines).contains("count: 2"));
}

#[tokio::test]
async fn attachment_read_byte_limits_are_positive_and_overflow_safe() {
    let Some(stored) = StoredAttachment::start(b"Hello", "text/plain").await else {
        return;
    };
    assert_error_contains(
        &stored.page(0, 0).await,
        "byte_limit must be greater than zero",
    );
    assert_page(&stored.page(1, usize::MAX).await, "ello", None);
    assert_page(&stored.page(usize::MAX, usize::MAX).await, "", None);
}

#[tokio::test]
async fn attachment_read_empty_byte_page_marks_end() {
    let Some(stored) = StoredAttachment::start(b"", "text/plain").await else {
        return;
    };
    let result = stored.read(json!({ "byte_limit": 1 })).await;
    assert_page(&result, "", None);
    assert!(metadata(&result).contains("offset 0 to 0/0"));
}
