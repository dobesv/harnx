use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use harnx_blob_store::media::{ensure_attachments_bucket, put_media};
use harnx_core::{cid_url::CidUrl, safety::WRITE_MAX_BYTES};
use sha2::{Digest, Sha256};

const SESSION: &str = "session-attachment";
const CLAIM: &str = "claim-attachment";

fn invocation(args: Value) -> ToolInvocation {
    ToolInvocation {
        tool: "write_attachment".into(),
        args,
        context: ToolInvocationContext {
            invoking_session_id: Some(harnx_core::session_identity::session_key(
                Some("coder"),
                SESSION,
            )),
            capabilities: BTreeSet::from(["dev.harnx.execution_context".into()]),
            ..Default::default()
        },
        cancel: CancellationToken::new(),
    }
}

async fn store_bytes(fixture: &ToolsetFixture, bytes: &[u8]) -> Result<String> {
    let store = ensure_attachments_bucket(fixture.metadata.jetstream(), 1).await?;
    let hash = harnx_core::crypto::hex_encode(&Sha256::digest(bytes));
    let url = format!("cid:media:_temp/abcdef/{hash}");
    put_media(
        &store,
        &CidUrl::parse(&url)?,
        bytes,
        "application/octet-stream",
    )
    .await?;
    Ok(url)
}

fn assert_attachment_call(call: &RecordedCall, sandbox_id: &str, path: &str, bytes: &[u8]) {
    assert_eq!(
        (
            call.sandbox_id.as_str(),
            call.endpoint.as_str(),
            call.tool.as_str()
        ),
        (sandbox_id, "http://10.0.0.8:3003/mcp", "write_bytes")
    );
    assert_eq!(
        &call.args,
        json!({"path": path, "data_base64": STANDARD.encode(bytes)})
            .as_object()
            .unwrap()
    );
}

fn assert_no_dispatch(fixture: &ToolsetFixture) {
    assert!(fixture.caller.calls.lock().is_empty());
    assert!(fixture.api.seen_ids.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_resolves_binary_and_empty_media_without_forwarding_public_args(
) -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start(SESSION, Some(CLAIM)).await? else {
        return Ok(());
    };
    for bytes in [b"\x89PNG\r\n\x1a\n\xff\0".as_slice(), b""] {
        let url = store_bytes(&fixture, bytes).await?;
        let result = fixture
            .fs
            .invoke_with_context(invocation(json!({"path": "/workspace/file", "url": url})))
            .await?;
        assert_eq!(result["content"][0]["text"], "proxied");
        let calls = fixture.caller.calls.lock();
        let call = calls.last().unwrap();
        assert_attachment_call(call, CLAIM, "/workspace/file", bytes);
        assert!(call.capabilities.contains("dev.harnx.execution_context"));
    }
    assert_eq!(fixture.caller.calls.lock().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_resolves_rendered_plan_markdown() -> Result<()> {
    use harnx_blob_store::plans::{create_document, serialize_plan, PlanDocument, PlanFrontMatter};

    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start(SESSION, Some(CLAIM)).await? else {
        return Ok(());
    };
    let jetstream = fixture.metadata.jetstream();
    let store = harnx_blob_store::ensure_plans_bucket(jetstream, 1).await?;
    let url = CidUrl::parse("cid:plan:_temp/abcdef/sandbox")?;
    let document = PlanDocument {
        front: PlanFrontMatter {
            id: url.to_string(),
            title: Some("Sandbox attachment".into()),
            created_at: "2026-11-02T00:00:00Z".into(),
            ..Default::default()
        },
        body: "Exact Markdown: café\r\n".into(),
    };
    create_document(&store, &url, &serialize_plan(&document)?).await?;
    let expected = harnx_blob_store::resolve(jetstream, &url).await?.bytes;
    fixture
        .fs
        .invoke_with_context(invocation(
            json!({"path": "/workspace/plan.md", "url": url.to_string()}),
        ))
        .await?;
    let calls = fixture.caller.calls.lock();
    assert_eq!(calls.len(), 1);
    assert_attachment_call(&calls[0], CLAIM, "/workspace/plan.md", &expected);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_uses_fs_backend_identity_with_custom_names_and_explicit_override(
) -> Result<()> {
    harnx_core::require_nextest();
    let names = SandboxToolsetNames {
        bash: "fs".into(),
        fs: "workspace".into(),
        sandbox: "review_sandbox".into(),
    };
    let Some(fixture) = ToolsetFixture::start_with_names(SESSION, Some(CLAIM), names).await? else {
        return Ok(());
    };
    let url = store_bytes(&fixture, b"custom backend").await?;
    fixture
        .fs
        .invoke(
            "write_attachment",
            json!({"path": "/workspace/file", "url": url, "sandbox_id": "claim-explicit"}),
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(
        bound_sandbox(&fixture.metadata, SESSION).await?.sandbox_id,
        CLAIM
    );
    let calls = fixture.caller.calls.lock();
    assert_eq!(fixture.fs.name(), "workspace");
    assert_eq!(calls.len(), 1);
    assert_attachment_call(
        &calls[0],
        "claim-explicit",
        "/workspace/file",
        b"custom backend",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_invalid_and_missing_cids_never_reach_pod() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start(SESSION, Some(CLAIM)).await? else {
        return Ok(());
    };
    for url in ["", " ", "cid:bad:value", "https://example.com"] {
        let error = fixture
            .fs
            .invoke_with_context(invocation(json!({"path": "/workspace/file", "url": url})))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolInvokeError::Recoverable(_)));
        assert!(
            error.to_string().contains("Invalid attachment URL:"),
            "{error}"
        );
    }
    let missing = format!("cid:media:_temp/abcdef/{}", "a".repeat(64));
    // Missing buckets and missing objects in existing buckets both stay recoverable.
    for existing_bucket in [false, true] {
        if existing_bucket {
            ensure_attachments_bucket(fixture.metadata.jetstream(), 1).await?;
            harnx_blob_store::ensure_plans_bucket(fixture.metadata.jetstream(), 1).await?;
        }
        for url in [missing.as_str(), "cid:plan:_temp/abcdef/missing"] {
            let error = fixture
                .fs
                .invoke_with_context(invocation(json!({"path": "/workspace/file", "url": url})))
                .await
                .unwrap_err();
            assert!(matches!(error, ToolInvokeError::Recoverable(_)));
            assert!(error.to_string().contains("not found:"), "{error}");
        }
    }
    assert_no_dispatch(&fixture);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_rejects_private_payload_and_malformed_args_before_dispatch() -> Result<()>
{
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start(SESSION, Some(CLAIM)).await? else {
        return Ok(());
    };
    let url = store_bytes(&fixture, b"trusted attachment").await?;
    for args in [
        json!({"path": "/workspace/file", "url": url, "data_base64": "YXR0YWNr"}),
        json!({"path": "/workspace/file"}),
        json!({"url": url}),
        json!({"path": null, "url": url}),
        json!({"path": 42, "url": url}),
        json!({"path": "/workspace/file", "url": null}),
        json!({"path": "/workspace/file", "url": 42}),
        json!({"path": "/workspace/file", "url": url, "sandbox_id": null}),
        json!([]),
    ] {
        let error = fixture
            .fs
            .invoke_with_context(invocation(args))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    }
    let error = fixture
        .fs
        .invoke(
            "write_bytes",
            json!({"path": "/workspace/file", "data_base64": ""}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(error.to_string().contains("unknown fs tool: write_bytes"));
    assert_no_dispatch(&fixture);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn attachment_write_oversize_is_recoverable_with_sizes_and_no_pod_call() -> Result<()> {
    harnx_core::require_nextest();
    let Some(fixture) = ToolsetFixture::start(SESSION, Some(CLAIM)).await? else {
        return Ok(());
    };
    let url = store_bytes(&fixture, b"oversize metadata").await?;
    let jetstream = fixture.metadata.jetstream();
    let stream = jetstream.get_stream("OBJ_harnx_attachments").await?;
    let message = stream
        .get_raw_message(stream.cached_info().state.last_sequence)
        .await?;
    let mut info: async_nats::jetstream::object_store::ObjectInfo =
        serde_json::from_slice(&message.payload)?;
    assert_eq!(info.name, CidUrl::parse(&url)?.kv_key());
    info.size = WRITE_MAX_BYTES + 1;
    // Reading any content would fail; the gateway must reject metadata first.
    info.digest = Some("SHA-256=invalid".into());
    jetstream
        .publish(message.subject, serde_json::to_vec(&info)?.into())
        .await?
        .await?;
    let error = fixture
        .fs
        .invoke_with_context(invocation(json!({"path": "/workspace/file", "url": url})))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert_eq!(
        error.to_string(),
        format!(
            "blob too large: {} bytes exceeds limit of {WRITE_MAX_BYTES} bytes",
            WRITE_MAX_BYTES + 1
        )
    );
    assert_no_dispatch(&fixture);
    Ok(())
}

#[path = "attachment_write/pod_errors.rs"]
mod pod_errors;
