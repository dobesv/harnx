//! Cross-replica regression for #2841: an upload received by one harnx-serve
//! process must let a different replica admit a prompt that references it,
//! even when the uploader's local attachments directory is gone (as on the
//! other replica, or after the uploading pod restarts).

use anyhow::{Context, Result};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

use crate::{access_agents::Fixture, common};

/// One `attachment` part, framed like the web UI's form upload.
fn multipart_attachment_body(
    boundary: &str,
    filename: &str,
    content_type: &str,
    data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"attachment\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

async fn create_session(fixture: &Fixture, agent: &str) -> Result<String> {
    let response = fixture
        .client
        .post(format!("{}/v1/agents/{agent}/sessions", fixture.base))
        .header("accept", "application/json")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await?;
    Ok(body["session_id"]
        .as_str()
        .context("created session id")?
        .to_string())
}

async fn upload_attachment(
    fixture: &Fixture,
    agent: &str,
    session: &str,
    data: &[u8],
) -> Result<String> {
    const BOUNDARY: &str = "cross-replica-boundary";
    let body = multipart_attachment_body(BOUNDARY, "chart.png", "image/png", data);
    let response = fixture
        .client
        .post(format!(
            "{}/v1/agents/{agent}/sessions/{session}/attachments",
            fixture.base
        ))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await?;
    let refs = body["attachment_refs"]
        .as_array()
        .context("upload refs array")?;
    assert_eq!(refs.len(), 1, "{body}");
    Ok(refs[0].as_str().context("first ref")?.to_string())
}

async fn prompt_with_attachment(
    fixture: &Fixture,
    agent: &str,
    session: &str,
    attachment_ref: &str,
) -> Result<Value> {
    let response = fixture
        .client
        .request(
            Method::POST,
            format!("{}/v1/agents/{agent}/sessions/{session}", fixture.base),
        )
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {"text": "what is in this image?", "attachment_refs": [attachment_ref]},
        }))
        .send()
        .await?;
    // Admission refusal (missing attachment) surfaces as HTTP 409 with a
    // JSON-RPC -32004 body, so a pre-fix regression shows up here, not below.
    assert_eq!(response.status(), StatusCode::OK, "prompt must be admitted");
    Ok(response.json().await?)
}

#[tokio::test]
async fn upload_prompts_are_admitted_on_replica_without_local_attachment_cache() -> Result<()> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server().await? else {
        return Ok(());
    };
    // Replica A receives the upload; it is the only pod that ever writes the
    // local attachments cache entry.
    let replica_a = Fixture::start_with_nats(None, nats.url()).await?;
    // Reserve the session first, the way the web UI does before the composer opens.
    let session = create_session(&replica_a, "allowed").await?;
    let image = b"cross replica png bytes";
    let attachment_ref = upload_attachment(&replica_a, "allowed", &session, image).await?;
    assert!(
        attachment_ref.starts_with("cid:media:allowed/"),
        "{attachment_ref}"
    );

    // Replica B never sees replica A's data directory. A pre-fix upload only
    // wrote A's local cache, so B's admission could not find the blob and the
    // prompt below returned 409; with the object-store write it is admitted.
    let replica_b = Fixture::start_with_nats(None, nats.url()).await?;
    let prompt = prompt_with_attachment(&replica_b, "allowed", &session, &attachment_ref).await?;
    assert_eq!(prompt["result"]["status"], "accepted", "{prompt}");
    assert!(
        prompt["result"]["run_id"].as_str().is_some(),
        "accepted prompt carries a run id: {prompt}"
    );
    Ok(())
}
