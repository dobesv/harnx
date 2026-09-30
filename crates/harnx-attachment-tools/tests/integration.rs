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
