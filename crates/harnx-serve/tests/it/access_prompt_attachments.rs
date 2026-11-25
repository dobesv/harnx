//! Prompt-time authorization of `cid:media` attachment refs against their
//! source session owner, exercised through the real HTTP JSON-RPC ingress
//! with access rules enabled (S1 follow-up).
//!
//! A ref names the agent and session that uploaded it. When access rules are
//! on, `session/prompt` must authorize the prompting caller for that source
//! session — in the destination cluster, with the same policy the route
//! guards use — before the session actor is spawned or the blob is looked
//! up. Everything else (same-session refs, an authorized caller, rules off)
//! keeps working.

use anyhow::{Context, Result};
use harnx_runtime::nats_session_log::NatsSessionLog;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

use crate::{access_agents::Fixture as HttpFixture, common, upload_cross_replica};

const RULES: &str = "rules:\n  - agents: [allowed]\n    scopes: [prompt]\n    users: [alice, carol]\n  - agents: [allowed]\n    scopes: [admin]\n    users: [admin]\n";
const JSON_RPC_ATTACHMENT_NOT_FOUND: i64 = -32005;
const JSON_RPC_REJECTED: i64 = -32004;

struct AttachmentFixture {
    http: HttpFixture,
    jetstream: async_nats::jetstream::Context,
    _nats: common::NatsServerHandle,
}

impl AttachmentFixture {
    async fn start(rules: Option<&str>) -> Result<Self> {
        harnx_core::require_nextest();
        let nats = common::spawn_nats_server()
            .await?
            .context("NATS required for prompt attachment authorization tests")?;
        let client = async_nats::connect(nats.url()).await?;
        let jetstream = async_nats::jetstream::new(client);
        let http = HttpFixture::start_with_nats(rules, nats.url()).await?;
        Ok(Self {
            http,
            jetstream,
            _nats: nats,
        })
    }

    /// Reserve a session the way the web UI does, as `user`, and return its id.
    async fn create_session(&self, user: &str) -> Result<String> {
        let response = self
            .http
            .client
            .post(format!("{}/v1/agents/allowed/sessions", self.http.base))
            .header("x-user", user)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await?;
        Ok(body["session_id"]
            .as_str()
            .context("created session id")?
            .to_string())
    }

    /// Reserve a session with no caller identity, for the rules-off fixture.
    async fn create_anonymous_session(&self) -> Result<String> {
        let response = self
            .http
            .client
            .post(format!("{}/v1/agents/allowed/sessions", self.http.base))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await?;
        Ok(body["session_id"]
            .as_str()
            .context("created session id")?
            .to_string())
    }

    async fn upload(&self, session: &str, user: &str, data: &[u8]) -> Result<String> {
        const BOUNDARY: &str = "prompt-attachment-boundary";
        let body = upload_cross_replica::multipart_attachment_body(
            BOUNDARY,
            "chart.png",
            "image/png",
            data,
        );
        let response = self
            .http
            .client
            .post(format!(
                "{}/v1/agents/allowed/sessions/{session}/attachments",
                self.http.base
            ))
            .header("x-user", user)
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

    async fn prompt(
        &self,
        session: &str,
        user: &str,
        refs: &[&str],
    ) -> Result<(StatusCode, Value)> {
        let refs: Vec<&str> = refs.to_vec();
        let response = self
            .http
            .client
            .request(
                Method::POST,
                format!("{}/v1/agents/allowed/sessions/{session}", self.http.base),
            )
            .header("x-user", user)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/prompt",
                "params": {"text": "what is in this image?", "attachment_refs": refs},
            }))
            .send()
            .await?;
        let status = response.status();
        Ok((status, response.json().await?))
    }

    async fn prompt_anonymous(&self, session: &str, refs: &[&str]) -> Result<(StatusCode, Value)> {
        let refs: Vec<&str> = refs.to_vec();
        let response = self
            .http
            .client
            .request(
                Method::POST,
                format!("{}/v1/agents/allowed/sessions/{session}", self.http.base),
            )
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/prompt",
                "params": {"text": "what is in this image?", "attachment_refs": refs},
            }))
            .send()
            .await?;
        let status = response.status();
        Ok((status, response.json().await?))
    }

    /// Durable entries of one session's transcript. An admission that never
    /// ran leaves this empty, proving no append and no worker activation:
    /// activation is only published for an appended, admitted prompt.
    async fn session_log_len(&self, session: &str) -> Result<usize> {
        let log = NatsSessionLog::for_agent(self.jetstream.clone(), "allowed", session);
        Ok(log.load_events_async().await?.len())
    }
}

/// Assert the generic denial shape: same status, code, and message for an
/// unauthorized source and an absent one.
fn assert_attachment_not_found(status: StatusCode, body: &Value) {
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["error"]["code"], JSON_RPC_ATTACHMENT_NOT_FOUND,
        "{body}"
    );
    assert_eq!(body["error"]["message"], "attachment not found", "{body}");
    assert!(body["result"].is_null(), "{body}");
}

/// Two sessions owned by different callers, each with one uploaded image.
struct TwoOwners {
    alice_session: String,
    carol_session: String,
    alice_ref: String,
    carol_ref: String,
}

async fn two_owner_sessions(f: &AttachmentFixture) -> Result<TwoOwners> {
    let alice_session = f.create_session("alice").await?;
    let carol_session = f.create_session("carol").await?;
    let alice_ref = f
        .upload(&alice_session, "alice", b"alice png bytes")
        .await?;
    let carol_ref = f
        .upload(&carol_session, "carol", b"carol png bytes")
        .await?;
    assert!(alice_ref.starts_with("cid:media:allowed/"), "{alice_ref}");
    assert!(carol_ref.starts_with("cid:media:allowed/"), "{carol_ref}");
    Ok(TwoOwners {
        alice_session,
        carol_session,
        alice_ref,
        carol_ref,
    })
}

#[tokio::test]
async fn unauthorized_foreign_media_ref_is_denied_without_log_append_or_activation() -> Result<()> {
    let f = AttachmentFixture::start(Some(RULES)).await?;
    let s = two_owner_sessions(&f).await?;

    // Alice can prompt her own session but must not reach Carol's upload.
    let (status, body) = f.prompt(&s.alice_session, "alice", &[&s.carol_ref]).await?;
    assert_attachment_not_found(status, &body);

    // No actor ran: neither transcript gained an entry, so nothing was
    // appended and no activation was published for either session.
    assert_eq!(f.session_log_len(&s.alice_session).await?, 0);
    assert_eq!(f.session_log_len(&s.carol_session).await?, 0);
    Ok(())
}

#[tokio::test]
async fn mixed_allowed_and_denied_refs_reject_whole_prompt() -> Result<()> {
    let f = AttachmentFixture::start(Some(RULES)).await?;
    let s = two_owner_sessions(&f).await?;

    let (status, body) = f
        .prompt(&s.alice_session, "alice", &[&s.alice_ref, &s.carol_ref])
        .await?;
    assert_attachment_not_found(status, &body);
    assert!(
        !matches!(
            body["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "the whole list must be rejected, not filtered: {body}"
    );
    assert_eq!(f.session_log_len(&s.alice_session).await?, 0);
    Ok(())
}

#[tokio::test]
async fn absent_source_session_shares_the_public_denial() -> Result<()> {
    let f = AttachmentFixture::start(Some(RULES)).await?;
    let s = two_owner_sessions(&f).await?;

    // A well-formed ref whose source session has no metadata record must be
    // indistinguishable from an unauthorized one, and must not depend on
    // whether any object exists under the ref.
    let absent =
        "cid:media:allowed/absent/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let (absent_status, absent_body) = f.prompt(&s.alice_session, "alice", &[absent]).await?;
    assert_attachment_not_found(absent_status, &absent_body);

    let (denied_status, denied_body) = f.prompt(&s.alice_session, "alice", &[&s.carol_ref]).await?;
    assert_attachment_not_found(denied_status, &denied_body);
    assert_eq!(absent_status, denied_status);
    assert_eq!(absent_body["error"], denied_body["error"]);
    assert_eq!(f.session_log_len(&s.alice_session).await?, 0);
    Ok(())
}

#[tokio::test]
async fn same_session_media_ref_is_admitted() -> Result<()> {
    let f = AttachmentFixture::start(Some(RULES)).await?;
    let s = two_owner_sessions(&f).await?;

    let (status, body) = f.prompt(&s.alice_session, "alice", &[&s.alice_ref]).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        matches!(
            body["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn cross_session_media_ref_is_admitted_for_authorized_callers() -> Result<()> {
    let f = AttachmentFixture::start(Some(RULES)).await?;
    let s = two_owner_sessions(&f).await?;
    let other_alice_session = f.create_session("alice").await?;
    let other_alice_ref = f
        .upload(&other_alice_session, "alice", b"other alice png bytes")
        .await?;

    // Alice owns both sessions, so her own cross-session ref passes.
    let (status, body) = f
        .prompt(&s.alice_session, "alice", &[&other_alice_ref])
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        matches!(
            body["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{body}"
    );

    // An admin-scope caller is authorized for Carol's session too.
    let (status, body) = f.prompt(&s.alice_session, "admin", &[&s.carol_ref]).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        matches!(
            body["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn rules_off_keeps_bearer_cid_behavior_for_foreign_refs() -> Result<()> {
    let f = AttachmentFixture::start(None).await?;
    let first = f.create_anonymous_session().await?;
    let second = f.create_anonymous_session().await?;
    let foreign_ref = f
        .upload(&second, "anonymous-user", b"foreign png bytes")
        .await?;

    // Without access rules a ref is a bearer capability, as before: a
    // cross-session ref is admitted and the blob must exist for it.
    let (status, body) = f.prompt_anonymous(&first, &[&foreign_ref]).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        matches!(
            body["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn malformed_media_ref_is_rejected_before_any_lookup() -> Result<()> {
    let f = AttachmentFixture::start(Some(RULES)).await?;
    let s = two_owner_sessions(&f).await?;

    // Malformed refs stay validation-style rejections (the same -32004 shape
    // the actor's admission used), not authorization denials, and run before
    // the actor can look anything up.
    let (status, body) = f
        .prompt(&s.alice_session, "alice", &["cid:media:not-a-hash"])
        .await?;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], JSON_RPC_REJECTED, "{body}");
    assert_eq!(f.session_log_len(&s.alice_session).await?, 0);
    Ok(())
}

const JSON_RPC_INVALID_PARAMS: i64 = -32602;

/// Over the documented cap, the whole list is a validation error: -32602,
/// HTTP 400, before any broker work.
#[tokio::test]
async fn too_many_media_refs_reject_the_prompt_with_a_validation_error() -> Result<()> {
    let f = AttachmentFixture::start(Some(RULES)).await?;
    let s = two_owner_sessions(&f).await?;

    let refs: Vec<String> = (0..33)
        .map(|i| format!("cid:media:allowed/{}/{:064x}", s.alice_session, i))
        .collect();
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let (status, body) = f.prompt(&s.alice_session, "alice", &refs).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], JSON_RPC_INVALID_PARAMS, "{body}");
    assert_eq!(
        body["error"]["message"], "attachment_refs: too many cid:media references (limit 32)",
        "{body}"
    );
    assert!(body["result"].is_null(), "{body}");
    assert_eq!(f.session_log_len(&s.alice_session).await?, 0);
    Ok(())
}

/// The cap boundary is admitted: 32 refs dedupe to one source read.
#[tokio::test]
async fn media_refs_at_the_documented_cap_are_admitted() -> Result<()> {
    let f = AttachmentFixture::start(Some(RULES)).await?;
    let s = two_owner_sessions(&f).await?;

    let refs: Vec<&str> = std::iter::repeat_n(s.alice_ref.as_str(), 32).collect();
    let (status, body) = f.prompt(&s.alice_session, "alice", &refs).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        matches!(
            body["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{body}"
    );
    Ok(())
}

/// A ref prompted at `allowed@remote` is authorized under the remote cluster
/// with the cluster-suffixed policy ref. The rules grant only
/// `allowed@remote`, not bare `allowed`, so a check that normalized the
/// source agent to the bare ref would deny this prompt. The two clusters
/// point at separate brokers, so the session metadata and media for the
/// source exist only on the remote broker: a check that read metadata via
/// the default cluster's URL would find nothing and deny, which pins
/// metadata-cluster selection on top of the policy normalization.
#[tokio::test]
async fn media_ref_source_is_authorized_in_a_nondefault_cluster() -> Result<()> {
    harnx_core::require_nextest();
    let default_nats = common::spawn_nats_server()
        .await?
        .context("NATS required for prompt attachment authorization tests")?;
    let remote_nats = common::spawn_nats_server()
        .await?
        .context("NATS required for prompt attachment authorization tests")?;
    let rules =
        "rules:\n  - agents: ['allowed@remote']\n    scopes: [prompt]\n    users: [alice]\n";
    let http =
        HttpFixture::start_with_nats_clusters(Some(rules), default_nats.url(), remote_nats.url())
            .await?;
    let remote_agent = "allowed%40remote";

    let response = http
        .client
        .post(format!("{}/v1/agents/{remote_agent}/sessions", http.base))
        .header("x-user", "alice")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let session = response.json::<Value>().await?["session_id"]
        .as_str()
        .context("remote session id")?
        .to_string();

    let body = upload_cross_replica::multipart_attachment_body(
        "remote-cluster-boundary",
        "chart.png",
        "image/png",
        b"remote png bytes",
    );
    let response = http
        .client
        .post(format!(
            "{}/v1/agents/{remote_agent}/sessions/{session}/attachments",
            http.base
        ))
        .header("x-user", "alice")
        .header(
            "content-type",
            "multipart/form-data; boundary=remote-cluster-boundary",
        )
        .body(body)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await?;
    let reference = body["attachment_refs"][0]
        .as_str()
        .context("remote upload ref")?
        .to_string();
    assert!(reference.starts_with("cid:media:allowed/"), "{reference}");

    let response = http
        .client
        .request(
            reqwest::Method::POST,
            format!("{}/v1/agents/{remote_agent}/sessions/{session}", http.base),
        )
        .header("x-user", "alice")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {"text": "what is in this image?", "attachment_refs": [reference]},
        }))
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        matches!(
            body["result"]["status"].as_str(),
            Some("accepted") | Some("enqueued")
        ),
        "{body}"
    );
    Ok(())
}
