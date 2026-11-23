//! Shared image-offload coverage: oversized successful replies with eligible
//! image blocks are delivered as `resource_link`s over session attachments.

use super::fixture::*;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use harnx_core::{
    attachments::cid_for_data_url_with_session,
    cid_url::{CidUrl, SessionRef},
};
use harnx_toolset::{ReplayAttempt, ToolErrorPayload, ToolReply, ToolRequest};
use serde_json::{json, Value};
use std::{sync::atomic::Ordering, time::Duration};

/// Deterministic pseudo-random bytes; the projection is MIME-agnostic, so a
/// real PNG decoder is not needed to test transport.
fn image_bytes(len: usize, salt: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i + salt as usize) % 251) as u8)
        .collect()
}

/// MCP-shaped successful result: one assistant-audience image block plus a
/// user-audience text summary — the wire shape `fs_read` and the fetch tools
/// emit for image responses.
fn image_result(bytes: &[u8], mime_type: &str, sibling_text: &str) -> Value {
    json!({
        "content": [
            {
                "type": "image",
                "data": STANDARD.encode(bytes),
                "mimeType": mime_type,
                "annotations": {"audience": ["assistant"]},
                "_meta": {"block": "kept"},
            },
            {
                "type": "text",
                "text": sibling_text,
                "annotations": {"audience": ["user"]},
            },
        ],
        "_meta": {"top": "kept"},
    })
}

fn image_block(result: &Value, index: usize) -> &Value {
    &result["content"][index]
}

fn preset(fixture: &Fixture, result: Value) {
    *fixture.toolset.preset.lock().unwrap() = Some(result);
}

/// The trusted owner `request(_, _, true)` attaches, mirroring the journal's
/// `attachment_owner`.
fn owner() -> SessionRef {
    SessionRef::new(Some("test/owner".into()), "out123".into()).expect("session ref")
}

/// Assert a `resource_link` block offloads exactly `bytes`: canonical CID over
/// the data URL, owner, MIME, size, preserved annotations/_meta, and a stored
/// blob that decodes back to the original bytes.
async fn assert_image_link(
    fixture: &Fixture,
    block: &Value,
    bytes: &[u8],
    mime_type: &str,
) -> Result<()> {
    assert_eq!(block["type"], json!("resource_link"));
    let expected = CidUrl::parse(&cid_for_data_url_with_session(
        &owner(),
        &format!("data:{mime_type};base64,{}", STANDARD.encode(bytes)),
    ))?;
    assert_eq!(block["uri"], json!(expected.to_string()));
    assert_eq!(
        block["name"],
        json!(format!("image.{}", &mime_type["image/".len()..]))
    );
    assert_eq!(block["mimeType"], json!(mime_type));
    assert_eq!(block["size"], json!(bytes.len()));
    assert_eq!(block["annotations"], json!({"audience": ["assistant"]}));
    let (stored, stored_mime) = stored_media(fixture, block["uri"].as_str().expect("uri")).await?;
    assert_eq!(
        stored, bytes,
        "stored blob must decode to the original bytes"
    );
    assert_eq!(stored_mime, mime_type);
    Ok(())
}

async fn stored_media(fixture: &Fixture, uri: &str) -> Result<(Vec<u8>, String)> {
    let url = CidUrl::parse(uri)?;
    let store = fixture
        .js
        .get_object_store(harnx_blob_store::ATTACHMENTS_BUCKET)
        .await?;
    harnx_blob_store::get_media(&store, &url)
        .await?
        .context("stored image blob")
}

async fn attachments_stream_messages(fixture: &Fixture) -> Result<u64> {
    let stream = format!("OBJ_{}", harnx_blob_store::ATTACHMENTS_BUCKET);
    let mut stream = fixture.js.get_stream(stream).await?;
    Ok(stream.info().await?.state.messages)
}

/// Require a successful delivery whose first content block is a stored
/// `resource_link` for `bytes`.
async fn assert_delivered_image_link(
    fixture: &Fixture,
    message: &async_nats::Message,
    bytes: &[u8],
    mime_type: &str,
) -> Result<()> {
    let delivered = reply(message)
        .result
        .expect("successful offloaded delivery");
    assert_image_link(fixture, image_block(&delivered, 0), bytes, mime_type).await?;
    Ok(())
}

/// Verify the durable row and the single execution every offload test relies on.
async fn assert_committed_single_call(
    fixture: &Fixture,
    request: &ToolRequest,
    message: &async_nats::Message,
    stored_cap: usize,
) -> Result<()> {
    fixture.check_stored(request, message, stored_cap).await?;
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

/// Call `request` and check it delivers one offloaded image end to end;
/// returns the delivered message and its decoded result for caller-specific
/// assertions.
async fn call_delivers_image_link(
    fixture: &Fixture,
    request: &ToolRequest,
    bytes: &[u8],
    mime_type: &str,
    stored_cap: usize,
) -> Result<(async_nats::Message, Value)> {
    let message = fixture.call(request).await?;
    assert_delivered_image_link(fixture, &message, bytes, mime_type).await?;
    assert_committed_single_call(fixture, request, &message, stored_cap).await?;
    let delivered = reply(&message)
        .result
        .expect("successful offloaded delivery");
    Ok((message, delivered))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_image_reply_offloads_to_resource_link() -> Result<()> {
    let mut fixture = Fixture::start(None, None).await?;
    assert_eq!(fixture.client.server_info().max_payload, 1_048_576);
    let bytes = image_bytes(900 * 1024, 0);
    let result = image_result(&bytes, "image/png", "summary kept");
    preset(&fixture, result.clone());
    let request = request("offload-single", 1, true);
    let (_, delivered) =
        call_delivers_image_link(&fixture, &request, &bytes, "image/png", 1_048_576).await?;
    let offloaded = image_block(&delivered, 0);
    assert_eq!(offloaded["_meta"], json!({"block": "kept"}));
    // Sibling order and non-image fields survive untouched.
    assert_eq!(
        image_block(&delivered, 1),
        &json!({"type": "text", "text": "summary kept", "annotations": {"audience": ["user"]}})
    );
    assert_eq!(delivered["_meta"], json!({"top": "kept"}));
    assert_eq!(delivered["content"].as_array().unwrap().len(), 2);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fitting_image_reply_stays_inline_without_bucket_access() -> Result<()> {
    let mut fixture = Fixture::start(None, None).await?;
    let bytes = image_bytes(2048, 0);
    let result = image_result(&bytes, "image/png", "small");
    preset(&fixture, result.clone());
    let request = request("fitting-inline", 1, true);
    let message = fixture.call(&request).await?;
    assert_eq!(
        reply(&message).result,
        Ok(result),
        "fitting reply is byte-identical"
    );
    assert_eq!(
        image_block(reply(&message).result.as_ref().unwrap(), 0)["type"],
        json!("image")
    );
    assert!(
        harnx_blob_store::media::optional_attachments_bucket(&fixture.js)
            .await?
            .is_none(),
        "a fitting reply must not touch attachment storage"
    );
    fixture.check_stored(&request, &message, 1_048_576).await?;
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_image_aggregate_overflow_offloads_all_eligible_images() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let first = image_bytes(6000, 0);
    let second = image_bytes(6000, 13);
    let result = json!({
        "content": [
            {
                "type": "image",
                "data": STANDARD.encode(&first),
                "mimeType": "image/png",
                "annotations": {"audience": ["assistant"]},
                "_meta": {"block": "first"},
            },
            {
                "type": "image",
                "data": STANDARD.encode(&second),
                "mimeType": "image/jpeg",
                "annotations": {"audience": ["assistant"]},
                "_meta": {"block": "second"},
            },
            {
                "type": "text",
                "text": "both linked",
                "annotations": {"audience": ["user"]},
            },
        ],
    });
    preset(&fixture, result.clone());
    let request = request("offload-multi", 1, true);
    let delivered = call_delivers_image_link(&fixture, &request, &first, "image/png", 8192)
        .await?
        .1;
    assert_eq!(
        image_block(&delivered, 0)["_meta"],
        json!({"block": "first"}),
        "per-block _meta preserved"
    );
    assert_image_link(&fixture, image_block(&delivered, 1), &second, "image/jpeg").await?;
    assert_eq!(
        image_block(&delivered, 1)["_meta"],
        json!({"block": "second"})
    );
    assert_eq!(
        image_block(&delivered, 2),
        &json!({"type": "text", "text": "both linked", "annotations": {"audience": ["user"]}}),
        "sibling order preserved"
    );
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn is_error_image_results_keep_generic_size_error() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let bytes = image_bytes(32 * 1024, 0);
    let mut result = image_result(&bytes, "image/png", "failed");
    result["isError"] = json!(true);
    preset(&fixture, result.clone());
    let request = request("is-error-image", 1, true);
    let message = fixture.call(&request).await?;
    assert!(
        matches!(
            reply(&message).result,
            Err(ToolErrorPayload::Recoverable(_))
        ),
        "isError results must take the existing generic path"
    );
    let original = retained(&fixture, &reply(&message)).await?;
    assert_eq!(original.result, Ok(result));
    fixture.check_stored(&request, &message, 8192).await?;
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_results_take_generic_size_path() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let result = json!({"content": "x".repeat(20_000), "isError": false});
    preset(&fixture, result.clone());
    let request = request("malformed-content", 1, true);
    let message = fixture.call(&request).await?;
    assert!(
        matches!(
            reply(&message).result,
            Err(ToolErrorPayload::Recoverable(_))
        ),
        "a malformed result must take the existing generic path"
    );
    let original = retained(&fixture, &reply(&message)).await?;
    assert_eq!(original.result, Ok(result));
    fixture.check_stored(&request, &message, 8192).await?;
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_owner_oversized_image_reply_is_generic_error_without_cid() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let bytes = image_bytes(32 * 1024, 0);
    let result = image_result(&bytes, "image/png", "orphan");
    preset(&fixture, result.clone());
    let mut request = request("no-owner-image", 1, false);
    request.args["parent_local_session_id"] = json!("forged");
    request.args["__harnx_parent_session_id"] = json!("forged");
    let message = fixture.call(&request).await?;
    let ToolErrorPayload::Recoverable(error) = reply(&message).result.unwrap_err() else {
        panic!("expected recoverable")
    };
    assert!(error.contains("not retained"));
    assert!(!error.contains("cid:"), "no CID without a trusted owner");
    fixture.check_stored(&request, &message, 8192).await?;
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    assert!(
        harnx_blob_store::media::optional_attachments_bucket(&fixture.js)
            .await?
            .is_none(),
        "no bucket without a trusted owner"
    );
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_upload_failure_is_fatal_and_does_not_claim_completion() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    harnx_blob_store::create_or_open_attachments_bucket(&fixture.js, 1).await?;
    let stream = fixture.js.get_stream("OBJ_harnx_attachments").await?;
    let mut config = stream.cached_info().config.clone();
    config.sealed = true;
    fixture.js.update_stream(config).await?;
    let bytes = image_bytes(32 * 1024, 0);
    let result = image_result(&bytes, "image/png", "never linked");
    preset(&fixture, result.clone());
    let request = request("sealed-image-upload", 1, true);
    let message = fixture.call(&request).await?;
    assert!(
        matches!(reply(&message).result, Err(ToolErrorPayload::Fatal(message)) if message.contains("store oversized tool image")),
        "a failed upload must be fatal, never a link or inline fallback"
    );
    assert!(fixture
        .journal
        .get(&request)
        .await?
        .unwrap()
        .reply
        .is_none());
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reduced_journal_headroom_still_offloads_images() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(4096)).await?;
    let bytes = image_bytes(16 * 1024, 0);
    let result = image_result(&bytes, "image/webp", "tight row");
    preset(&fixture, result.clone());
    let request = request("reduced-headroom", 1, true);
    call_delivers_image_link(&fixture, &request, &bytes, "image/webp", 4096).await?;
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_image_link_replays_identically_after_restart_without_reexecution() -> Result<()>
{
    let mut fixture = Fixture::start(None, None).await?;
    let bytes = image_bytes(900 * 1024, 0);
    let result = image_result(&bytes, "image/png", "replayed");
    preset(&fixture, result.clone());
    let request = request("replay-image", 1, true);
    let (message, _) =
        call_delivers_image_link(&fixture, &request, &bytes, "image/png", 1_048_576).await?;
    fixture.restart().await?;
    let mut replay = request.clone();
    replay.replay = Some(ReplayAttempt {
        attempt: 1,
        requested_by: "replacement-worker".into(),
    });
    assert_eq!(fixture.call(&replay).await?.payload, message.payload);
    assert_eq!(
        fixture.toolset.calls.load(Ordering::SeqCst),
        1,
        "no replay reexecution"
    );
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ineligible_image_blocks_are_never_linked() -> Result<()> {
    let mut fixture = Fixture::start(None, Some(8192)).await?;
    let audio = image_bytes(32 * 1024, 0);
    let result = json!({
        "content": [
            {
                "type": "image",
                "data": STANDARD.encode(&audio),
                "mimeType": "audio/mpeg",
            },
            {
                "type": "image",
                "data": "not base64 !!!",
                "mimeType": "image/png",
            },
            {
                "type": "text",
                "text": "no eligible block",
            },
        ],
    });
    preset(&fixture, result.clone());
    let request = request("ineligible-blocks", 1, true);
    let message = fixture.call(&request).await?;
    assert!(
        matches!(
            reply(&message).result,
            Err(ToolErrorPayload::Recoverable(_))
        ),
        "no eligible block means the generic path"
    );
    let original = retained(&fixture, &reply(&message)).await?;
    let content = original.result.unwrap()["content"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(
        content[0]["type"],
        json!("image"),
        "non-image MIME stays inline"
    );
    assert_eq!(content[0]["mimeType"], json!("audio/mpeg"));
    assert_eq!(
        content[1]["data"],
        json!("not base64 !!!"),
        "invalid base64 stays inline"
    );
    // Only the retained application/json original was stored, never a media blob.
    assert_eq!(attachments_stream_messages(&fixture).await?, 2);
    fixture.check_stored(&request, &message, 8192).await?;
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_journal_complete_is_unchanged_without_image_projection() -> Result<()> {
    let mut fixture = Fixture::start(None, None).await?;
    let bytes = image_bytes(900 * 1024, 0);
    let result = image_result(&bytes, "image/png", "raw path");
    let request = request("raw-complete", 1, true);
    fixture
        .journal
        .record(&request, ("echo", fixture.scope.as_str(), "____test"))
        .await?;
    let original = ToolReply {
        input_required: None,
        call_id: request.call_id.clone(),
        result: Ok(result),
        final_progress: None,
    };
    let saved = fixture.journal.complete(&request, original.clone()).await?;
    assert!(
        matches!(&saved.result, Err(ToolErrorPayload::Recoverable(_))),
        "raw complete keeps the bounded size error, never a projection"
    );
    assert_eq!(
        fixture.journal.completed_reply(&request).await?,
        Some(saved.clone())
    );
    assert_eq!(
        fixture
            .journal
            .get(&request)
            .await?
            .unwrap()
            .reply
            .as_ref()
            .unwrap()
            .result,
        saved.result
    );
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 0);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_size_rejection_retries_once_and_commits_image_projection() -> Result<()> {
    let mut fixture = Fixture::start(None, None).await?;
    let mut config = fixture.journal_config().await?;
    config.max_message_size = 8192;
    fixture.js.update_stream(config).await?;
    // Server journal handle predates the changed limit: the preflight lets the
    // inline reply through and the real 10054 triggers the single retry.
    let bytes = image_bytes(64 * 1024, 0);
    let result = image_result(&bytes, "image/png", "typed retry");
    preset(&fixture, result.clone());
    let request = request("typed-retry-image", 1, true);
    let (message, terminal_sizes) = call_observing_terminal_writes(&fixture, &request).await?;
    assert_eq!(
        terminal_sizes.len(),
        2,
        "exactly one typed-size retry: {terminal_sizes:?}"
    );
    assert!(
        terminal_sizes[0] > 8192,
        "first attempt is the inline reply"
    );
    assert!(terminal_sizes[1] < 8192, "retry commits the projection");
    assert_delivered_image_link(&fixture, &message, &bytes, "image/png").await?;
    assert_committed_single_call(&fixture, &request, &message, 8192).await?;
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_size_rejection_without_fitting_projection_falls_back_to_generic_error() -> Result<()>
{
    let mut fixture = Fixture::start(None, None).await?;
    let mut config = fixture.journal_config().await?;
    config.max_message_size = 8192;
    fixture.js.update_stream(config).await?;
    let bytes = image_bytes(64 * 1024, 0);
    let result = image_result(&bytes, "image/png", &"sibling ".repeat(1200));
    preset(&fixture, result.clone());
    let request = request("typed-retry-nofit", 1, true);
    let message = fixture.call(&request).await?;
    assert!(
        matches!(
            reply(&message).result,
            Err(ToolErrorPayload::Recoverable(_))
        ),
        "a projection that does not fit falls back to the generic size error"
    );
    let original = retained(&fixture, &reply(&message)).await?;
    assert_eq!(original.result, Ok(result));
    fixture.check_stored(&request, &message, 8192).await?;
    assert_eq!(fixture.toolset.calls.load(Ordering::SeqCst), 1);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_acknowledged_before_cas_conflict_are_reused_not_reput() -> Result<()> {
    let fixture = Fixture::start(None, None).await?;
    let bytes = image_bytes(8 * 1024 * 1024, 0);
    let result = image_result(&bytes, "image/png", "single upload");
    preset(&fixture, result.clone());
    let mut request = request("cas-reuse", 1, true);
    request.args["park"] = json!(true);
    let mut object_writes = fixture.client.subscribe("$O.harnx_attachments.>").await?;
    crate::common::round_trip(&fixture.client).await?;
    let pending = fixture.call(&request);
    tokio::pin!(pending);
    tokio::select! {
        _ = fixture.toolset.entered.notified() => {},
        result = &mut pending => anyhow::bail!("tool did not park: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(60)) => anyhow::bail!("tool did not enter"),
    }
    fixture.toolset.release.notify_one();
    // The first object-store chunk proves the journal row was already read for
    // this CAS attempt, so bumping its revision now forces a conflict after
    // the upload is acknowledged. The retry must reuse the stored blob instead
    // of re-putting it.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let write = object_writes.next().await.context("object writes closed")?;
            if write.subject.contains(".C.") {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    let entry = fixture.store.entry(key(&request)).await?.context("row")?;
    fixture
        .store
        .update(key(&request), entry.value.clone(), entry.revision)
        .await?;
    let message = pending.await?;
    assert_delivered_image_link(&fixture, &message, &bytes, "image/png").await?;
    assert_committed_single_call(&fixture, &request, &message, 1_048_576).await?;
    // One upload's worth of messages: 8 MiB at the 128 KiB chunk limit is 64
    // chunk messages plus the object meta message. A re-put would double it.
    assert_eq!(attachments_stream_messages(&fixture).await?, 65);
    // Release the immutable borrow held by the pinned call before the fixture
    // is dropped.
    Ok(())
}
