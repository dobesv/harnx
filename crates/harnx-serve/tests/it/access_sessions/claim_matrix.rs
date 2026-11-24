//! T3 matrix for the deferred session-claim path that needs no test seam:
//! the hidden non-creating cases, orphan transcripts, whitespace IDs,
//! attachment denial, and deletion interplay. The deterministic races and
//! registered-actor cases live in-crate (`src/session_claim_tests.rs`)
//! because only an in-process server can arm the claim gate.

use super::*;

use std::time::{Duration, Instant};

fn resume_entry() -> Value {
    json!({
        "interruptId": "tool-1",
        "status": "resolved",
        "payload": {"approved": true, "reason": "test"}
    })
}

fn rpc_body(method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string()
}

fn prompt_body(params: Value) -> String {
    rpc_body("session/prompt", params)
}

fn sse_body(messages: Value, extra: Value) -> String {
    let mut body = json!({
        "messages": messages,
        "tools": [],
        "context": [],
        "forwardedProps": {}
    });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            body.insert(key.clone(), value.clone());
        }
    }
    body.to_string()
}

/// No canonical metadata record and no transcript stream for `id`.
async fn assert_no_session_state(f: &Fixture, id: &str) -> Result<()> {
    assert!(
        f.store.get_for_agent(id, "allowed").await?.is_none(),
        "no canonical metadata for '{id}'"
    );
    assert_no_transcript_stream(f, id).await
}

async fn assert_no_transcript_stream(f: &Fixture, id: &str) -> Result<()> {
    let log = NatsSessionLog::for_agent(f.jetstream.clone(), "allowed", id);
    let name = harnx_runtime::nats_session_log::stream_name_for_session(log.storage_key());
    assert!(
        f.jetstream.get_stream(&name).await.is_err(),
        "no transcript stream for '{id}'"
    );
    Ok(())
}

async fn transcript_len(f: &Fixture, id: &str) -> Result<usize> {
    let log = NatsSessionLog::for_agent(f.jetstream.clone(), "allowed", id);
    Ok(log.load_events_async().await?.len())
}

fn stream_name(f: &Fixture, id: &str) -> String {
    let log = NatsSessionLog::for_agent(f.jetstream.clone(), "allowed", id);
    harnx_runtime::nats_session_log::stream_name_for_session(log.storage_key())
}

/// One seeded transcript message for building orphan sessions.
fn seeded_message(text: &str) -> SessionLogEntry {
    SessionLogEntry::Message {
        id: Some("seed".into()),
        role: MessageRole::User,
        content: MessageContent::Text(text.into()),
        timestamp: None,
        fence_token: None,
    }
}

/// A `session/prompt` RPC with `params`, sent to `id` as `user` and
/// compared byte-for-byte against the same request to bob's `foreign`
/// session as alice: the exact guard 404.
async fn assert_prompt_hidden(f: &Fixture, id: &str, user: &str, params: Value) -> Result<()> {
    let rpc = json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt",
        "params": params});
    let response = f
        .session(id, user)
        .request(Method::POST, "")
        .json(&rpc)
        .send()
        .await?;
    let baseline = f
        .session("foreign", "alice")
        .request(Method::POST, "")
        .json(&rpc)
        .send()
        .await?;
    assert_hidden_response(response, baseline).await
}

/// R4: on an unseen ID under rules, every non-creating request — non-prompt
/// RPC methods with valid params, resume-only and mixed prompts, blank
/// prompts, promptless SSE, SSE with resume decisions, malformed bodies,
/// unsupported content types, deeper subroutes, and non-POST methods —
/// answers the exact foreign-session 404 and leaves no metadata, stream, or
/// actor state behind.
// qual:allow(complexity, max_function_lines=300) reason: "table-driven hidden-case matrix: 28 request shapes as one table with a single shared comparison loop; splitting the table across tests would hide which shapes are covered."
#[tokio::test]
async fn access_nats_claim_non_creating_requests_on_unseen_ids_create_nothing() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let before = f.store.list().await?.len();
    let unseen = "matrix-unseen";
    let hitl_params = json!({
        "request": {"invocation_id": "i", "tool_round_seq": 1, "request_seq": 2,
                    "tool_call_id": "call"},
        "approved": true
    });
    // (label, method, path suffix, accept, content-type, body)
    let user_message = json!([{"role": "user", "content": "hello"}]);
    let cases: Vec<(&str, Method, String, &str, &str, String)> = vec![
        // Non-prompt RPC methods with valid params.
        (
            "get",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body("session/get", json!({})),
        ),
        (
            "history",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body("session/history", json!({"page_size": 10})),
        ),
        (
            "cancel",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body("session/cancel", json!({})),
        ),
        (
            "compact",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body("session/compact", json!({})),
        ),
        (
            "mark_read",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body("session/mark_read", json!({})),
        ),
        (
            "mark_unread",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body("session/mark_unread", json!({})),
        ),
        (
            "hitl",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body("session/hitl_decision", json!({"approved": true})),
        ),
        (
            "hitl_exact",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body("session/hitl_decision_exact", hitl_params.clone()),
        ),
        (
            "interaction_response",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body(
                "session/interaction_response",
                json!({"interaction_id": "i", "request_seq": 1, "response": {"action": "acknowledge", "data": {}}}),
            ),
        ),
        (
            "surface_action",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            rpc_body(
                "session/surface_action",
                json!({"surface_id": "s", "publish_seq": 1, "response": {"action": "acknowledge", "data": {}}}),
            ),
        ),
        // Prompts that don't qualify as creating. The `"text": ""` payloads
        // let these reach the rows the design says rejects them (review
        // C-new-1): the resume entry disqualifies at row d, the denied ref
        // at row g — without it, a missing required `text` field would fail
        // the row-b parse instead of exercising those rows.
        (
            "resume_only",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            prompt_body(json!({"text": "", "resume": [resume_entry()]})),
        ),
        (
            "mixed_resume",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            prompt_body(json!({"text": "hello", "resume": [resume_entry()]})),
        ),
        (
            "blank",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            prompt_body(json!({"text": "   "})),
        ),
        // SSE inputs that don't qualify as creating, and malformed bodies.
        (
            "sse_promptless",
            Method::POST,
            String::new(),
            "text/event-stream",
            "application/json",
            sse_body(json!([]), json!({})),
        ),
        (
            "sse_assistant_only",
            Method::POST,
            String::new(),
            "text/event-stream",
            "application/json",
            sse_body(json!([{"role": "assistant", "content": "hi"}]), json!({})),
        ),
        (
            "sse_blank_user",
            Method::POST,
            String::new(),
            "text/event-stream",
            "application/json",
            sse_body(json!([{"role": "user", "content": "   "}]), json!({})),
        ),
        (
            "sse_user_then_assistant",
            Method::POST,
            String::new(),
            "text/event-stream",
            "application/json",
            sse_body(
                json!([
                    {"role": "user", "content": "hello"},
                    {"role": "assistant", "content": "hi"},
                ]),
                json!({}),
            ),
        ),
        (
            "sse_resume_decisions",
            Method::POST,
            String::new(),
            "text/event-stream",
            "application/json",
            sse_body(user_message.clone(), json!({"resume": [resume_entry()]})),
        ),
        (
            "sse_malformed",
            Method::POST,
            String::new(),
            "text/event-stream",
            "application/json",
            "{".to_string(),
        ),
        (
            "rpc_malformed",
            Method::POST,
            String::new(),
            "application/json",
            "application/json",
            "{".to_string(),
        ),
        (
            "unsupported_content_type",
            Method::POST,
            String::new(),
            "application/json",
            "text/plain",
            prompt_body(json!({"text": "hello"})),
        ),
        // Deeper subroutes: the guard never defers them.
        (
            "attachments",
            Method::POST,
            "/attachments".into(),
            "application/json",
            "application/json",
            "{}".to_string(),
        ),
        (
            "metadata",
            Method::POST,
            "/metadata".into(),
            "application/json",
            "application/json",
            "{}".to_string(),
        ),
        (
            "events",
            Method::POST,
            "/events".into(),
            "application/json",
            "application/json",
            "{}".to_string(),
        ),
        // Non-POST methods on the item route.
        (
            "get_item",
            Method::GET,
            String::new(),
            "application/json",
            "application/json",
            prompt_body(json!({"text": "hello"})),
        ),
        (
            "put_item",
            Method::PUT,
            String::new(),
            "application/json",
            "application/json",
            prompt_body(json!({"text": "hello"})),
        ),
        (
            "patch_item",
            Method::PATCH,
            String::new(),
            "application/json",
            "application/json",
            prompt_body(json!({"text": "hello"})),
        ),
        (
            "delete_item",
            Method::DELETE,
            String::new(),
            "application/json",
            "application/json",
            prompt_body(json!({"text": "hello"})),
        ),
    ];
    for (label, method, suffix, accept, content_type, body) in cases {
        let path = format!("/v1/agents/allowed/sessions/{unseen}{suffix}");
        let hidden = f
            .http
            .client
            .request(method.clone(), format!("{}{path}", f.http.base))
            .header("x-user", "bob")
            .header("accept", accept)
            .header("content-type", content_type)
            .body(body.clone())
            .send()
            .await?;
        let foreign_path = format!("/v1/agents/allowed/sessions/foreign{suffix}");
        let baseline = f
            .http
            .client
            .request(method, format!("{}{foreign_path}", f.http.base))
            .header("x-user", "alice")
            .header("accept", accept)
            .header("content-type", content_type)
            .body(body)
            .send()
            .await?;
        assert_hidden_response(hidden, baseline)
            .await
            .with_context(|| label.to_string())?;
    }
    assert_no_session_state(&f, unseen).await?;
    assert_eq!(f.store.list().await?.len(), before);
    Ok(())
}

/// R10: a transcript stream without canonical metadata is not claimable,
/// whether it holds entries or is an empty leftover of a failed orphan-GC
/// cleanup with media and plans still present.
#[tokio::test]
async fn access_nats_claim_orphan_transcripts_are_not_claimable() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    // A nonempty orphan transcript: entries but no metadata.
    let orphan_full = NatsSessionLog::new(
        f.jetstream.clone(),
        harnx_core::session_identity::session_key(Some("allowed"), "orphan-full"),
    )
    .with_replicas(1);
    orphan_full
        .append_event_async(&seeded_message("orphan seed"))
        .await?;
    assert_prompt_hidden(&f, "orphan-full", "alice", json!({"text": "hello orphan"})).await?;
    assert!(
        f.store
            .get_for_agent("orphan-full", "allowed")
            .await?
            .is_none(),
        "nothing claimed for the orphan"
    );
    assert_eq!(transcript_len(&f, "orphan-full").await?, 1);

    // An empty orphan stream left behind by a failed or paused orphan-GC
    // cleanup, with media and plans still present.
    let orphan_empty = NatsSessionLog::new(
        f.jetstream.clone(),
        harnx_core::session_identity::session_key(Some("allowed"), "orphan-empty"),
    )
    .with_replicas(1);
    orphan_empty
        .append_event_async(&seeded_message("soon purged"))
        .await?;
    f.jetstream
        .get_stream(&stream_name(&f, "orphan-empty"))
        .await?
        .purge()
        .filter(harnx_runtime::nats_session_log::subject_for_session(
            &harnx_core::session_identity::session_key(Some("allowed"), "orphan-empty"),
        ))
        .await?;
    let session_ref = SessionRef::new(Some("allowed".into()), "orphan-empty".into())?;
    let media = harnx_blob_store::media::ensure_attachments_bucket(&f.jetstream, 1).await?;
    let media_cid = CidUrl::Media {
        session: session_ref.clone(),
        hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
    };
    harnx_blob_store::media::put_media(&media, &media_cid, b"png bytes", "image/png").await?;
    let plans = harnx_blob_store::plans::ensure_plans_bucket(&f.jetstream, 1).await?;
    let plan_cid = CidUrl::Plan {
        session: session_ref,
        slug: "leftover".into(),
        item: harnx_core::cid_url::PlanItem::Index,
    };
    harnx_blob_store::plans::create_document(&plans, &plan_cid, "# Leftover\n").await?;
    assert_prompt_hidden(&f, "orphan-empty", "alice", json!({"text": "hello empty"})).await?;
    assert!(
        f.store
            .get_for_agent("orphan-empty", "allowed")
            .await?
            .is_none(),
        "nothing claimed for the empty orphan"
    );
    Ok(())
}

/// Design step 1: whitespace-padded and whitespace-only IDs never defer, so
/// a creating prompt keeps the guard's 404 and no generated-ID session
/// appears anywhere.
#[tokio::test]
async fn access_nats_claim_whitespace_ids_stay_hidden() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let before = f.store.list().await?.len();
    let prompt = json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt",
        "params": {"text": "hello"}});
    for encoded in ["%20padded%20", "%20%20"] {
        let hidden = f
            .request(
                Method::POST,
                &format!("/v1/agents/allowed/sessions/{encoded}"),
                "bob",
            )
            .json(&prompt)
            .send()
            .await?;
        let baseline = f
            .session("foreign", "alice")
            .request(Method::POST, "")
            .json(&prompt)
            .send()
            .await?;
        assert_hidden_response(hidden, baseline)
            .await
            .with_context(|| encoded.to_string())?;
    }
    for raw in [" padded ", "  ", "padded"] {
        assert!(
            f.store.get_for_agent(raw, "allowed").await?.is_none(),
            "no session for '{raw}'"
        );
    }
    assert_eq!(
        f.store.list().await?.len(),
        before,
        "no generated-ID session may appear"
    );
    Ok(())
}

/// R6: a denied `cid:media` reference on an otherwise creating prompt is a
/// pre-reservation refusal — the guard's 404, nothing claimed — for both a
/// text-plus-attachment and an attachment-only prompt.
#[tokio::test]
async fn access_nats_claim_denied_media_reference_is_hidden() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let foreign_ref =
        "cid:media:allowed/foreign/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    for (label, params) in [
        (
            "text_plus_ref",
            json!({"text": "what is this", "attachment_refs": [foreign_ref]}),
        ),
        // `"text": ""` keeps the ref-only prompt inside the creating
        // classification (row b parse passes on refs alone), so the denial
        // it hits is row g's — the pre-reservation preflight — not a
        // parse rejection (review C-new-1).
        (
            "ref_only",
            json!({"text": "", "attachment_refs": [foreign_ref]}),
        ),
    ] {
        let response = f
            .session("deny-media", "alice")
            .request(Method::POST, "")
            .json(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                "params": params}),
            )
            .send()
            .await?;
        let baseline = f
            .session("foreign", "alice")
            .request(Method::POST, "")
            .json(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                "params": params}),
            )
            .send()
            .await?;
        assert_hidden_response(response, baseline)
            .await
            .with_context(|| label.to_string())?;
    }
    assert_no_session_state(&f, "deny-media").await?;
    Ok(())
}

/// Review C-new-1, row d confirmed from both sides: the resume-carrying
/// prompt stays hidden, and the identical prompt minus the resume entry
/// creates the session — so it is the resume decision that disqualifies,
/// not the parse. Both carry the same nonblank text (CodeRabbit nitpick):
/// a blank one could hide for the content reason alone.
#[tokio::test]
async fn access_nats_claim_resume_entry_is_what_disqualifies() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let params = json!({"text": "please continue", "resume": [resume_entry()]});
    assert_prompt_hidden(&f, "resume-row", "alice", params).await?;
    assert_no_session_state(&f, "resume-row").await?;

    let created = f
        .session("row-d-created", "alice")
        .request(Method::POST, "")
        .json(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"text": "please continue"}}),
        )
        .send()
        .await?;
    assert_eq!(created.status(), StatusCode::OK);
    let record = f
        .store
        .get_for_agent("row-d-created", "allowed")
        .await?
        .expect("created metadata");
    assert_eq!(
        session_properties(&record.metadata)?.text("user_id"),
        Some("alice")
    );
    Ok(())
}

/// Review C9, the row-g positive case: an authorized `cid:media` reference
/// — a prompt-scope user's own media — is a creating prompt by itself. The
/// attachment-only prompt creates the session with the caller as owner,
/// which also confirms the denied `ref_only` case above reaches row g
/// (a parse rejection could never create).
#[tokio::test]
async fn access_nats_claim_authorized_attachment_only_prompt_creates() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let owned_ref =
        "cid:media:allowed/owned1/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let response = f
        .session("attach-only", "alice")
        .request(Method::POST, "")
        .json(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt",
                "params": {"text": "", "attachment_refs": [owned_ref]}}),
        )
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await?;
    assert!(
        matches!(
            created["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{created}"
    );
    let record = f
        .store
        .get_for_agent("attach-only", "allowed")
        .await?
        .expect("attachment-only prompt created the session");
    assert_eq!(
        session_properties(&record.metadata)?.text("user_id"),
        Some("alice"),
        "the caller owns the attachment-only creation"
    );
    Ok(())
}

/// Review C7: over SSE the prompt-scope requirement is the collection
/// route's visible 403 (row f, R5) — an admin-only caller's creating run is
/// refused before reservation, with nothing claimed.
#[tokio::test]
async fn access_nats_claim_admin_scope_over_sse_is_403() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let body = sse_body(json!([{"role": "user", "content": "hello"}]), json!({}));
    let response = f
        .http
        .client
        .post(format!(
            "{}/v1/agents/allowed/sessions/sse-admin",
            f.http.base
        ))
        .header("x-user", "admin")
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let text = response.text().await?;
    assert!(
        text.contains("session creation requires prompt scope"),
        "the SSE refusal must match the collection route's 403: {text}"
    );
    assert_no_session_state(&f, "sse-admin").await?;
    Ok(())
}

/// R9: a deletion that fails mid-cascade keeps the canonical record — purged
/// last since T0 — with its owner, so another prompt-scope user cannot claim
/// the ID: bob's creating prompt gets the exact guard 404 and claims nothing.
///
/// The failure is T0's own injected one: the plans cleanup inside the real
/// `delete_remote_session` cascade cannot decode a plan document, so the
/// deletion errors after the transcript step and before the canonical purge.
#[tokio::test]
async fn access_nats_claim_partial_deletion_keeps_old_owner() -> Result<()> {
    let Some((f, config, _plans)) = poisoned_alice_session("zombie").await? else {
        return Ok(());
    };

    assert_cascade_fails_at_plans(&config, "zombie").await?;

    // The transcript stream is gone but the canonical record still owns the
    // ID, so bob's prompt stays behind the exact guard 404.
    assert!(
        f.jetstream
            .get_stream(&stream_name(&f, "zombie"))
            .await
            .is_err(),
        "the cascade already deleted the transcript"
    );
    assert_prompt_hidden(&f, "zombie", "bob", json!({"text": "my zombie now"})).await?;
    let record = f
        .store
        .get_for_agent("zombie", "allowed")
        .await?
        .expect("canonical record survives the failed cascade");
    assert_eq!(
        session_properties(&record.metadata)?.text("user_id"),
        Some("alice")
    );
    Ok(())
}

/// R9, the rest of the deletion story: the same poisoned cascade is repaired
/// and re-run to completion — canonical record purged last — and after that
/// bob's first prompt to the same ID creates a fresh session that bob owns.
#[tokio::test]
async fn access_nats_claim_after_full_deletion_new_user_recreates() -> Result<()> {
    let Some((f, config, plans)) = poisoned_alice_session("recycled").await? else {
        return Ok(());
    };

    assert_cascade_fails_at_plans(&config, "recycled").await?;

    // Repair the failing step and re-run the same entry point to completion.
    plans
        .put(&poisoned_plan_key("recycled"), "# Valid document".into())
        .await?;
    let deleted =
        harnx_runtime::nats_admin::delete_remote_session(&config, "local", "allowed", "recycled")
            .await?;
    assert!(!deleted.stream_deleted, "the retry starts streamless");
    assert!(
        f.store
            .get_for_agent("recycled", "allowed")
            .await?
            .is_none(),
        "canonical record purged once the cascade completes"
    );

    // bob's first prompt to the recycled ID creates a fresh session he owns.
    let response = f
        .session("recycled", "bob")
        .request(Method::POST, "")
        .json(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"text": "fresh start"}}),
        )
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await?;
    assert!(
        matches!(
            created["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{created}"
    );
    let record = f
        .store
        .get_for_agent("recycled", "allowed")
        .await?
        .expect("recreated canonical metadata");
    assert_eq!(
        session_properties(&record.metadata)?.text("user_id"),
        Some("bob")
    );
    Ok(())
}

/// C1: an over-cap declared body hides both a missing and a foreign ID
/// behind the same guard 404, and the precheck's refusal is final before
/// any body byte: with `Expect: 100-continue` and the oversized length
/// declared, the body stays withheld and the first bytes back are the
/// final 404, never an interim 100 (CodeRabbit 4677217448). Sending the
/// 20 MiB body against the early refusal is what the Windows runner
/// aborted with 10053, so this test never sends it.
#[tokio::test]
async fn access_nats_claim_oversized_body_is_hidden_for_foreign_and_missing() -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    // The guard's cap is the crate's MAX_UPLOAD_BYTES (20 MiB); the const
    // is crate-private, so the boundary is spelled out here.
    let oversized_length = 20 * 1024 * 1024 + 1;
    let authority = f
        .http
        .base
        .trim_start_matches("http://")
        .split('/')
        .next()
        .context("fixture base authority")?
        .to_string();
    let mut responses = Vec::new();
    for id in ["oversized-missing", "foreign"] {
        let mut stream = tokio::net::TcpStream::connect(&authority).await?;
        let head = format!(
            "POST /v1/agents/allowed/sessions/{id} HTTP/1.1\r\n\
             Host: {authority}\r\nx-user: alice\r\naccept: application/json\r\n\
             content-type: application/json\r\nContent-Length: {oversized_length}\r\n\
             Expect: 100-continue\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await?;
        stream.flush().await?;
        // With the body withheld, the declared-length precheck alone must
        // answer: the response's first line is the final 404, not an
        // interim `100 Continue` followed by something else.
        let response = read_raw_response(&mut stream, Duration::from_secs(15)).await?;
        assert!(
            response.starts_with(b"HTTP/1.1 404"),
            "the over-cap declared length must answer the final 404 first for {id}: {response:?}"
        );
        responses.push(response);
    }
    fn without_date(response: &[u8]) -> Vec<Vec<u8>> {
        response
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.starts_with(b"date:"))
            .map(<[u8]>::to_vec)
            .collect()
    }
    assert_eq!(
        without_date(&responses[0]),
        without_date(&responses[1]),
        "the over-cap refusal must be identical for foreign and missing"
    );
    assert_no_session_state(&f, "oversized-missing").await?;
    Ok(())
}

/// Read one complete HTTP response over a raw socket — the headers through
/// the blank line, then the `content-length` body — bounded by `budget`.
/// A complete response, not a clean close, is what ends the read: the
/// over-cap precheck answers before the request body is consumed, so the
/// server closes once done and the close can arrive as a reset, which
/// must not fail the test.
async fn read_raw_response(
    stream: &mut tokio::net::TcpStream,
    budget: Duration,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + budget;
    let mut response = Vec::new();
    while !response.windows(4).any(|window| window == b"\r\n\r\n") {
        read_raw_chunk(stream, &mut response, deadline).await?;
    }
    let headers_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("response headers terminator")?
        + 4;
    let length = String::from_utf8_lossy(&response[..headers_end])
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while response.len() < headers_end + length {
        read_raw_chunk(stream, &mut response, deadline).await?;
    }
    Ok(response)
}

/// One bounded read into the in-progress raw response.
async fn read_raw_chunk(
    stream: &mut tokio::net::TcpStream,
    response: &mut Vec<u8>,
    deadline: Instant,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(
        !remaining.is_zero(),
        "response never arrived within budget: {response:?}"
    );
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(remaining, stream.read(&mut buf))
        .await
        .context("response read timed out")?
        .context("response read failed")?;
    anyhow::ensure!(n > 0, "connection closed before the response: {response:?}");
    response.extend_from_slice(&buf[..n]);
    Ok(())
}

/// The shared opening of both R9 deletion tests: a rules-on fixture with
/// alice's session seeded under `id` and its plan document poisoned, plus a
/// config pointing the real deletion cascade at the fixture's broker.
async fn poisoned_alice_session(
    id: &str,
) -> Result<
    Option<(
        Fixture,
        harnx_runtime::config::Config,
        async_nats::jetstream::kv::Store,
    )>,
> {
    let Some(f) = fixture(true).await? else {
        return Ok(None);
    };
    let config = deletion_config(&f);
    f.seed(
        &SessionRef::new(Some("allowed".into()), id.to_string())?,
        Some("alice"),
    )
    .await?;
    let plans = poison_plans(&f, id).await?;
    Ok(Some((f, config, plans)))
}

/// Run the real deletion cascade against `id` (after `poison_plans`) and
/// require it to fail at the plans step, before the canonical purge.
async fn assert_cascade_fails_at_plans(
    config: &harnx_runtime::config::Config,
    id: &str,
) -> Result<()> {
    let error = harnx_runtime::nats_admin::delete_remote_session(config, "local", "allowed", id)
        .await
        .expect_err("plan cleanup must fail the deletion");
    assert!(
        error.downcast_ref::<std::string::FromUtf8Error>().is_some(),
        "the cascade must fail at the plans step: {error:#}"
    );
    Ok(())
}

/// A minimal config against the fixture's own broker, the entry point
/// `harnx delete session` uses for remote deletions.
fn deletion_config(f: &Fixture) -> harnx_runtime::config::Config {
    harnx_runtime::config::Config {
        nats_servers: vec![harnx_runtime::config::NatsServerConfig {
            user_id: None,
            name: "local".to_string(),
            url: f._nats.url().to_string(),
            token: None,
            replicas: None,
            tls: None,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            ignore_discovered_servers: None,
            agents: vec![],
        }],
        ..Default::default()
    }
}

/// The plan key whose undecodable document fails the cascade's plans step.
fn poisoned_plan_key(id: &str) -> String {
    let storage_key = harnx_core::session_identity::session_key(Some("allowed"), id);
    format!("plan/{storage_key}/test/index.md")
}

/// Seed the session's plan document as invalid UTF-8, the same injected
/// failure `nats_session_delete.rs` uses: the real plans cleanup refuses to
/// decode it, so the deletion cascade errors before the canonical purge.
async fn poison_plans(f: &Fixture, id: &str) -> Result<async_nats::jetstream::kv::Store> {
    let plans = harnx_blob_store::plans::ensure_plans_bucket(&f.jetstream, 1).await?;
    plans.put(&poisoned_plan_key(id), vec![0xff].into()).await?;
    Ok(plans)
}
