//! Session access through real HTTP dispatch and canonical NATS metadata.

use anyhow::{Context, Result};
use harnx_core::{
    cid_url::{CidUrl, SessionRef},
    message::{ImageUrl, MessageContent, MessageContentPart, MessageRole},
    session::SessionLogEntry,
};
use harnx_runtime::{
    nats_session_log::NatsSessionLog,
    nats_session_metadata::{
        session_properties, SessionInitializer, SessionMetadata, SessionMetadataStore,
    },
};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

use crate::{access_agents::Fixture as HttpFixture, common};

const RULES: &str = "rules:\n  - agents: [allowed, blocked]\n    scopes: [prompt]\n    users: [alice, bob]\n  - agents: [allowed, blocked]\n    scopes: [admin]\n    users: [admin]\n";

struct Fixture {
    http: HttpFixture,
    store: SessionMetadataStore,
    jetstream: async_nats::jetstream::Context,
    _nats: common::NatsServerHandle,
}

async fn fixture(rules_on: bool) -> Result<Option<Fixture>> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server().await? else {
        return Ok(None);
    };
    let client = async_nats::connect(nats.url()).await?;
    let jetstream = async_nats::jetstream::new(client);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    let http = HttpFixture::start_with_nats(rules_on.then_some(RULES), nats.url()).await?;
    let f = Fixture {
        http,
        store,
        jetstream,
        _nats: nats,
    };
    for (id, user) in [
        ("owned1", Some("alice")),
        ("foreign", Some("bob")),
        ("legacy", None),
    ] {
        f.seed(&SessionRef::new(Some("allowed".into()), id.into())?, user)
            .await?;
    }
    Ok(Some(f))
}

impl Fixture {
    fn request(&self, method: Method, path: &str, user: &str) -> RequestBuilder {
        self.http
            .client
            .request(method, format!("{}{path}", self.http.base))
            .header("x-user", user)
            .header("accept", "application/json")
    }

    fn session<'a>(&'a self, id: &'a str, user: &'a str) -> SessionClient<'a> {
        SessionClient {
            fixture: self,
            id,
            user,
        }
    }

    async fn seed(&self, session: &SessionRef, user: Option<&str>) -> Result<String> {
        let agent = session.agent.as_deref().context("named session")?;
        let id = &session.session_id;
        let mut initializer = SessionInitializer::named(agent, Default::default());
        if let Some(user) = user {
            initializer = initializer.with_user_id(user);
        }
        self.store
            .create(&SessionMetadata::new(id, initializer))
            .await?;
        let cid = CidUrl::Media {
            session: session.clone(),
            hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        };
        let media = harnx_blob_store::media::ensure_attachments_bucket(&self.jetstream, 1).await?;
        harnx_blob_store::media::put_media(&media, &cid, b"png bytes", "image/png").await?;
        NatsSessionLog::new(
            self.jetstream.clone(),
            harnx_core::session_identity::session_key(Some(agent), id),
        )
        .with_replicas(1)
        .append_event_async(&SessionLogEntry::Message {
            id: Some("image".into()),
            role: MessageRole::User,
            content: MessageContent::Array(vec![MessageContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: cid.to_string(),
                },
            }]),
            timestamp: None,
            fence_token: None,
        })
        .await?;
        Ok(cid.to_string())
    }

    async fn json(&self, path: &str, user: &str) -> Result<Value> {
        let response = self.request(Method::GET, path, user).send().await?;
        assert_eq!(response.status(), StatusCode::OK, "{user} {path}");
        Ok(response.json().await?)
    }
}

struct SessionClient<'a> {
    fixture: &'a Fixture,
    id: &'a str,
    user: &'a str,
}

impl SessionClient<'_> {
    fn request(&self, method: Method, suffix: &str) -> RequestBuilder {
        self.fixture.request(
            method,
            &format!("/v1/agents/allowed/sessions/{}{suffix}", self.id),
            self.user,
        )
    }

    async fn rpc(&self, method: &str) -> Result<Value> {
        let response = self
            .request(Method::POST, "")
            .json(
                &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {"text": "hello"}}),
            )
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK, "{} {method}", self.user);
        let body: Value = response.json().await?;
        assert!(body.get("result").is_some(), "{body}");
        Ok(body)
    }

    async fn json(&self, suffix: &str) -> Result<Value> {
        let response = self.request(Method::GET, suffix).send().await?;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{} {} {suffix}",
            self.id,
            self.user
        );
        Ok(response.json().await?)
    }

    async fn assert_rpc_hidden(&self, unknown: &Self) -> Result<()> {
        for method in [
            "session/get",
            "session/prompt",
            "session/hitl_decision",
            "session/cancel",
            "session/compact",
            "session/mark_read",
            "session/mark_unread",
        ] {
            let rpc =
                json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {"text": "hello"}});
            let hidden = self.request(Method::POST, "").json(&rpc).send().await?;
            let unknown = unknown.request(Method::POST, "").json(&rpc).send().await?;
            assert_hidden_response(hidden, unknown)
                .await
                .with_context(|| format!("{} {method}", self.id))?;
        }
        Ok(())
    }

    async fn assert_metadata_operations(&self) -> Result<()> {
        self.json("/metadata").await?;
        let patch = self
            .request(Method::PATCH, "/metadata")
            .json(&json!({"title": {"value": "updated", "manual": true}}))
            .send()
            .await?;
        assert_eq!(patch.status(), StatusCode::OK);
        let put = self
            .request(Method::PUT, "/metadata/extensions/test")
            .json(&json!({"value": 1}))
            .send()
            .await?;
        assert_eq!(put.status(), StatusCode::OK);
        let delete = self
            .request(Method::DELETE, "/metadata/extensions/test")
            .send()
            .await?;
        assert_eq!(delete.status(), StatusCode::OK);
        Ok(())
    }

    async fn assert_representations(&self) -> Result<()> {
        let html = self
            .request(Method::GET, "")
            .header("accept", "text/html")
            .send()
            .await?;
        assert_eq!(html.status(), StatusCode::OK);
        assert!(html.headers()["content-type"]
            .to_str()?
            .starts_with("text/html"));
        let events = self
            .request(Method::GET, "/events")
            .header("accept", "text/event-stream")
            .send()
            .await?;
        assert_eq!(events.status(), StatusCode::OK);
        assert_eq!(events.headers()["content-type"], "text/event-stream");
        drop(events);
        let blob = self
            .request(
                Method::GET,
                &format!("/attachments/{}", encoded_cid(self.id)),
            )
            .send()
            .await?;
        assert_eq!(blob.status(), StatusCode::OK);
        Ok(())
    }
}

async fn assert_hidden_response(
    hidden: reqwest::Response,
    unknown: reqwest::Response,
) -> Result<()> {
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND, "{}", hidden.url());
    assert_eq!(hidden.headers()["content-type"], "application/json");
    assert_eq!(hidden.headers()["access-control-allow-origin"], "*");
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND, "{}", unknown.url());
    assert_eq!(hidden.text().await?, unknown.text().await?);
    Ok(())
}

fn ids(summaries: &Value) -> Vec<String> {
    let mut result: Vec<_> = summaries
        .as_array()
        .expect("summaries")
        .iter()
        .map(|s| s["session_id"].as_str().expect("session id").to_string())
        .collect();
    result.sort();
    result
}

fn encoded_cid(id: &str) -> String {
    percent_encoding::utf8_percent_encode(&format!("cid:media:allowed/{id}/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"), percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[tokio::test]
async fn access_nats_prompt_user_creates_reads_and_prompts_own_session() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let response = f
        .request(Method::POST, "/v1/agents/allowed/sessions", "alice")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value = response.json().await?;
    let id = created["session_id"]
        .as_str()
        .context("reserved session id")?;
    let record = f
        .store
        .get_for_agent(id, "allowed")
        .await?
        .expect("canonical metadata");
    assert_eq!(
        session_properties(&record.metadata)?.text("user_id"),
        Some("alice")
    );
    assert!(f.session(id, "alice").json("").await?.is_array());
    f.session(id, "alice").rpc("session/get").await?;
    f.session(id, "alice").rpc("session/mark_read").await?;
    f.session(id, "alice").rpc("session/mark_unread").await?;
    let prompted = f.session(id, "alice").rpc("session/prompt").await?;
    assert!(
        matches!(
            prompted["result"]["status"].as_str(),
            Some("accepted" | "enqueued")
        ),
        "{prompted}"
    );
    let response = f
        .session("owned1", "alice")
        .request(
            Method::GET,
            &format!("/attachments/{}", encoded_cid("owned1")),
        )
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await?.as_ref(), b"png bytes");
    Ok(())
}

#[tokio::test]
async fn access_nats_foreign_and_legacy_sessions_are_hidden_on_every_subroute() -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    let routes = [
        (Method::GET, "", "application/json"),
        (Method::GET, "", "text/html"),
        (Method::POST, "", "text/event-stream"),
        (Method::GET, "/events", "text/event-stream"),
        (Method::GET, "/metadata", "application/json"),
        (Method::PATCH, "/metadata", "application/json"),
        (Method::PUT, "/metadata/extensions/test", "application/json"),
        (
            Method::DELETE,
            "/metadata/extensions/test",
            "application/json",
        ),
        (Method::GET, "/attachments/not-a-cid", "application/json"),
        (Method::POST, "/attachments", "application/json"),
        (Method::DELETE, "/attachments", "application/json"),
        (Method::GET, "/unknown-subroute", "application/json"),
    ];
    for id in ["foreign", "legacy"] {
        for (method, suffix, accept) in &routes {
            let hidden = f
                .session(id, "alice")
                .request(method.clone(), suffix)
                .header("accept", *accept)
                .header("content-type", "application/json")
                .body("{")
                .send()
                .await?;
            let unknown = f
                .session("missing", "alice")
                .request(method.clone(), suffix)
                .header("accept", *accept)
                .header("content-type", "application/json")
                .body("{")
                .send()
                .await?;
            assert_hidden_response(hidden, unknown)
                .await
                .with_context(|| format!("{id} {method}"))?;
        }
        f.session(id, "alice")
            .assert_rpc_hidden(&f.session("missing", "alice"))
            .await?;
    }
    assert!(f.store.get_for_agent("missing", "allowed").await?.is_none());
    assert_eq!(f.store.list().await?.len(), 3);
    Ok(())
}

#[tokio::test]
async fn access_nats_admin_reads_operates_foreign_and_legacy_sessions_but_cannot_create(
) -> Result<()> {
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    for id in ["foreign", "legacy"] {
        assert!(f.session(id, "admin").json("").await?.is_array());
        let session = f.session(id, "admin");
        session.assert_metadata_operations().await?;
        session.assert_representations().await?;
        f.session(id, "admin").rpc("session/get").await?;
        let prompted = f.session(id, "admin").rpc("session/prompt").await?;
        assert!(
            matches!(
                prompted["result"]["status"].as_str(),
                Some("accepted" | "enqueued")
            ),
            "{prompted}"
        );
    }
    for path in [
        "/v1/agents/allowed/sessions",
        "/v1/agents/allowed/sessions/",
    ] {
        let denied = f.request(Method::POST, path, "admin").send().await?;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let body: Value = denied.json().await?;
        assert_eq!(
            body["error"]["message"],
            "session creation requires prompt scope"
        );
    }
    for user in ["alice", "admin"] {
        let implicit = f.session("new-id", user).request(Method::POST, "")
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":"session/prompt", "params":{"text":"hello"}})).send().await?;
        assert_eq!(implicit.status(), StatusCode::NOT_FOUND);
    }
    assert!(f.store.get_for_agent("new-id", "allowed").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn access_nats_lists_filter_before_pagination_and_session_keys_are_agent_scoped() -> Result<()>
{
    let Some(f) = fixture(true).await? else {
        return Ok(());
    };
    assert_eq!(
        ids(&f.json("/v1/agents/allowed/sessions", "alice").await?),
        ["owned1"]
    );
    assert_eq!(
        ids(&f.json("/v1/agents/allowed/sessions", "bob").await?),
        ["foreign"]
    );
    assert_eq!(
        ids(&f.json("/v1/agents/allowed", "alice").await?["sessions"]),
        ["owned1"]
    );
    assert_eq!(
        ids(&f.json("/v1/agents/allowed/sessions", "admin").await?),
        ["foreign", "legacy", "owned1"]
    );
    let page = f
        .json("/v1/agents/allowed/sessions?limit=1", "alice")
        .await?;
    assert_eq!(ids(&page["sessions"]), ["owned1"]);
    assert!(page["next_cursor"].is_null());
    let page = f.json("/v1/agents/allowed/sessions?limit=1", "bob").await?;
    assert_eq!(ids(&page["sessions"]), ["foreign"]);
    assert!(page["next_cursor"].is_null());
    f.seed(
        &SessionRef::new(Some("allowed".into()), "shared".into())?,
        Some("bob"),
    )
    .await?;
    f.seed(
        &SessionRef::new(Some("blocked".into()), "shared".into())?,
        Some("alice"),
    )
    .await?;
    let denied = f
        .session("shared", "alice")
        .request(Method::GET, "/metadata")
        .send()
        .await?;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    f.json("/v1/agents/blocked/sessions/shared/metadata", "alice")
        .await?;
    assert_eq!(
        ids(&f.json("/v1/agents/blocked/sessions", "alice").await?),
        ["shared"]
    );
    Ok(())
}

#[tokio::test]
async fn access_nats_rules_off_preserve_foreign_legacy_and_implicit_session_behavior() -> Result<()>
{
    let Some(f) = fixture(false).await? else {
        return Ok(());
    };
    for id in ["foreign", "legacy"] {
        f.json(
            &format!("/v1/agents/allowed/sessions/{id}/metadata"),
            "alice",
        )
        .await?;
    }
    assert_eq!(
        ids(&f.json("/v1/agents/allowed/sessions", "alice").await?),
        ["foreign", "legacy", "owned1"]
    );
    let create = f
        .request(Method::POST, "/v1/agents/allowed/sessions", "admin")
        .send()
        .await?;
    assert_eq!(create.status(), StatusCode::CREATED);
    let implicit = f.session("implicit", "admin").rpc("session/prompt").await?;
    assert!(
        matches!(
            implicit["result"]["status"].as_str(),
            Some("accepted" | "enqueued")
        ),
        "{implicit}"
    );
    Ok(())
}
