//! Alias authorization through the real HTTP server and isolated NATS metadata.
use crate::{
    access_agents::{Fixture as HttpFixture, MembershipSettings},
    common,
};
use anyhow::{Context, Result};
use harnx_runtime::nats_session_metadata::{
    session_properties, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

const ALIASES: &str = "- name: Display\n  identities: [alice, bob, bob]\n- name: Overlap\n  identities: [bob, carol]\n- name: Role\n  identities: [operator, supervisor]\n- name: Group\n  identities: [team, admins]\n- name: Empty\n  identities: ['', bob]\n";
const RULES: &str = "rules:\n  - agents: [allowed]\n    users: [bob, carol, unrelated, unknown]\n  - agents: [blocked]\n    users: [carol]\n  - agents: [allowed]\n    groups: [team]\n  - agents: [allowed]\n    roles: [supervisor]\n    scopes: [admin]\n  - agents: [allowed]\n    groups: [admins]\n    scopes: [admin]\n";

struct Fixture {
    http: HttpFixture,
    store: SessionMetadataStore,
    _nats: common::NatsServerHandle,
}

impl Fixture {
    async fn start(rules: Option<&str>, aliases: Option<&str>) -> Result<Self> {
        harnx_core::require_nextest();
        let nats = common::spawn_nats_server()
            .await?
            .context("NATS required for alias integration")?;
        let client = async_nats::connect(nats.url()).await?;
        let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;
        let http = HttpFixture::start_with_user_aliases(
            rules,
            Some(nats.url()),
            MembershipSettings {
                config: "serve_group_headers: [x-groups]\nserve_role_headers: [x-roles]\n",
                ..Default::default()
            },
            aliases,
        )
        .await?;
        let fixture = Self {
            http,
            store,
            _nats: nats,
        };
        for (id, owner) in [
            ("alias-owner", Some("bob")),
            ("raw-owner", Some("alice")),
            ("overlap-owner", Some("carol")),
            ("unrelated-owner", Some("unrelated")),
            ("unknown-owner", Some("unknown")),
            ("anonymous", None),
        ] {
            let mut initializer = SessionInitializer::named("allowed", Default::default());
            if let Some(owner) = owner {
                initializer = initializer.with_user_id(owner);
            }
            fixture
                .store
                .create(&SessionMetadata::new(id, initializer))
                .await?
                .context("new fixture session")?;
        }
        Ok(fixture)
    }

    fn request(&self, method: Method, path: &str, user: Option<&str>) -> RequestBuilder {
        let request = self
            .http
            .client
            .request(method, format!("{}{path}", self.http.base))
            .header("accept", "application/json");
        match user {
            Some(user) => request.header("x-user", user),
            None => request,
        }
    }

    async fn json(&self, path: &str, user: Option<&str>) -> Result<Value> {
        let response = self.request(Method::GET, path, user).send().await?;
        assert_eq!(response.status(), StatusCode::OK, "{path} {user:?}");
        Ok(response.json().await?)
    }

    async fn owner(&self, id: &str) -> Result<Option<String>> {
        let record = self
            .store
            .get_for_agent(id, "allowed")
            .await?
            .context("canonical session")?;
        Ok(session_properties(&record.metadata)?
            .text("user_id")
            .map(str::to_owned))
    }

    async fn assert_session_status(
        &self,
        id: &str,
        user: Option<&str>,
        status: StatusCode,
    ) -> Result<()> {
        for suffix in ["", "/metadata"] {
            let response = self
                .request(
                    Method::GET,
                    &format!("/v1/agents/allowed/sessions/{id}{suffix}"),
                    user,
                )
                .send()
                .await?;
            assert_eq!(response.status(), status, "{id} {user:?} {suffix}");
        }
        let response = self
            .request(
                Method::POST,
                &format!("/v1/agents/allowed/sessions/{id}"),
                user,
            )
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":"session/get"}))
            .send()
            .await?;
        assert_eq!(response.status(), status, "{id} {user:?} RPC");
        if status == StatusCode::OK {
            let body: Value = response.json().await?;
            assert!(body.get("result").is_some(), "{body}");
        }
        Ok(())
    }
}

fn ids(summaries: &Value) -> Vec<String> {
    let mut ids: Vec<_> = summaries
        .as_array()
        .expect("session summaries")
        .iter()
        .map(|summary| {
            summary["session_id"]
                .as_str()
                .expect("session id")
                .to_owned()
        })
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn alias_expanded_caller_can_access_alias_owned_session() -> Result<()> {
    let f = Fixture::start(Some(RULES), Some(ALIASES)).await?;
    let agents = f.json("/v1/agents", Some("alice")).await?;
    assert!(agents["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| agent["name"] == "allowed"));
    assert!(!agents["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| agent["name"] == "blocked"));
    let sessions = f.json("/v1/agents/allowed/sessions", Some("alice")).await?;
    assert_eq!(ids(&sessions), ["alias-owner", "raw-owner"]);
    let agent = f.json("/v1/agents/allowed", Some("alice")).await?;
    assert_eq!(ids(&agent["sessions"]), ["alias-owner", "raw-owner"]);
    let page = f
        .json("/v1/agents/allowed/sessions?limit=1", Some("alice"))
        .await?;
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1);
    assert!(page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session["user_id"] == "alice" || session["user_id"] == "bob"));
    f.assert_session_status("alias-owner", Some("alice"), StatusCode::OK)
        .await?;
    f.assert_session_status("unrelated-owner", Some("alice"), StatusCode::NOT_FOUND)
        .await?;
    f.assert_session_status("anonymous", Some("alice"), StatusCode::NOT_FOUND)
        .await?;
    let response = f.request(Method::POST, "/v1/agents/allowed/sessions/alias-owner", Some("alice"))
        .json(&json!({"jsonrpc":"2.0", "id":2, "method":"session/prompt", "params":{"text":"alias continuation"}})).send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await?;
    assert!(
        matches!(
            body["result"]["status"].as_str(),
            Some("accepted" | "enqueued")
        ),
        "{body}"
    );
    assert_eq!(f.owner("alias-owner").await?.as_deref(), Some("bob"));
    Ok(())
}

#[tokio::test]
async fn unmatched_caller_retains_singleton_identity() -> Result<()> {
    let f = Fixture::start(Some(RULES), Some(ALIASES)).await?;
    assert_eq!(
        ids(&f
            .json("/v1/agents/allowed/sessions", Some("unknown"))
            .await?),
        ["unknown-owner"]
    );
    f.assert_session_status("unknown-owner", Some("unknown"), StatusCode::OK)
        .await?;
    f.assert_session_status("alias-owner", Some("unknown"), StatusCode::NOT_FOUND)
        .await?;
    for user in ["Display", "Alice", "nobody"] {
        let response = f
            .request(Method::GET, "/v1/agents/allowed", Some(user))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{user}");
    }
    Ok(())
}

#[tokio::test]
async fn overlap_entries_dont_merge_first_match_wins() -> Result<()> {
    let f = Fixture::start(Some(RULES), Some(ALIASES)).await?;
    assert_eq!(
        ids(&f.json("/v1/agents/allowed/sessions", Some("bob")).await?),
        ["alias-owner", "raw-owner"]
    );
    assert_eq!(
        ids(&f.json("/v1/agents/allowed/sessions", Some("carol")).await?),
        ["alias-owner", "overlap-owner"]
    );
    f.assert_session_status("overlap-owner", Some("alice"), StatusCode::NOT_FOUND)
        .await?;
    // Expanding stored Alice to Bob would incorrectly admit Carol.
    f.assert_session_status("raw-owner", Some("carol"), StatusCode::NOT_FOUND)
        .await?;
    for (user, status) in [
        ("alice", StatusCode::NOT_FOUND),
        ("bob", StatusCode::NOT_FOUND),
        ("carol", StatusCode::OK),
    ] {
        let response = f
            .request(Method::GET, "/v1/agents/blocked", Some(user))
            .send()
            .await?;
        assert_eq!(response.status(), status, "{user}");
    }
    Ok(())
}

#[tokio::test]
async fn session_create_preserves_raw_user_id() -> Result<()> {
    let f = Fixture::start(Some(RULES), Some(ALIASES)).await?;
    let response = f
        .request(Method::POST, "/v1/agents/allowed/sessions", Some("alice"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await?;
    let id = body["session_id"].as_str().context("created session id")?;
    assert_eq!(f.owner(id).await?.as_deref(), Some("alice"));
    f.assert_session_status(id, Some("bob"), StatusCode::OK)
        .await?;
    let summaries = f.json("/v1/agents/allowed/sessions", Some("alice")).await?;
    assert_eq!(
        summaries
            .as_array()
            .unwrap()
            .iter()
            .find(|summary| summary["session_id"] == id)
            .unwrap()["user_id"],
        "alice"
    );
    Ok(())
}

#[tokio::test]
async fn missing_users_file_is_noop() -> Result<()> {
    let f = Fixture::start(Some(RULES), None).await?;
    assert!(!f.http.user_aliases_path().try_exists()?);
    assert_eq!(
        ids(&f.json("/v1/agents/allowed/sessions", Some("bob")).await?),
        ["alias-owner"]
    );
    f.assert_session_status("raw-owner", Some("bob"), StatusCode::NOT_FOUND)
        .await?;
    let response = f
        .request(Method::GET, "/v1/agents/allowed", Some("alice"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Creating the file after startup must not turn aliases on.
    std::fs::write(f.http.user_aliases_path(), ALIASES)?;
    let response = f
        .request(Method::GET, "/v1/agents/allowed", Some("alice"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn malformed_users_file_fails_startup() -> Result<()> {
    for yaml in [
        "---",
        "- name: missing-identities",
        "[unclosed",
        "users: []",
    ] {
        let result = HttpFixture::start_with_user_aliases(
            Some(RULES),
            None,
            MembershipSettings::default(),
            Some(yaml),
        )
        .await;
        let error = result
            .err()
            .context("invalid users.yaml must fail binary startup")?;
        let message = format!("{error:#}");
        assert!(message.contains("users.yaml"), "{message}");
        assert!(message.contains("user aliases"), "{message}");
        assert!(message.contains("exited during startup"), "{message}");
    }
    Ok(())
}

#[tokio::test]
async fn aliases_without_access_rules_keep_http_unrestricted_and_anonymous() -> Result<()> {
    let f = Fixture::start(None, Some(ALIASES)).await?;
    let expected = [
        "alias-owner",
        "anonymous",
        "overlap-owner",
        "raw-owner",
        "unknown-owner",
        "unrelated-owner",
    ];
    for caller in [Some("alice"), Some("unknown"), None] {
        assert_eq!(
            ids(&f.json("/v1/agents/allowed/sessions", caller).await?),
            expected
        );
        f.assert_session_status("unrelated-owner", caller, StatusCode::OK)
            .await?;
        f.assert_session_status("anonymous", caller, StatusCode::OK)
            .await?;
    }
    let response = f
        .request(Method::POST, "/v1/agents/allowed/sessions", None)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await?;
    assert_eq!(f.owner(body["session_id"].as_str().unwrap()).await?, None);
    Ok(())
}

#[tokio::test]
async fn aliases_do_not_expand_memberships_or_replace_user_ownership() -> Result<()> {
    let f = Fixture::start(Some(RULES), Some(ALIASES)).await?;
    for (groups, roles, allowed) in [
        ("alice, team", "", false),
        ("", "operator", false),
        ("", "supervisor", true),
        ("admins", "", true),
    ] {
        let response = f
            .request(
                Method::GET,
                "/v1/agents/allowed/sessions/alias-owner/metadata",
                Some("unknown"),
            )
            .header("x-groups", groups)
            .header("x-roles", roles)
            .send()
            .await?;
        assert_eq!(
            response.status(),
            if allowed {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            }
        );
    }
    // Team must stay a prompt group, not expand to the admin group.
    let response = f
        .request(
            Method::GET,
            "/v1/agents/allowed/sessions/unrelated-owner/metadata",
            Some("alice"),
        )
        .header("x-groups", "team")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = f
        .request(Method::GET, "/v1/agents/allowed", None)
        .header("x-roles", "supervisor")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(f.owner("alias-owner").await?.as_deref(), Some("bob"));
    Ok(())
}

#[tokio::test]
async fn aliases_use_immutable_startup_snapshot_and_require_identity_with_rules() -> Result<()> {
    let f = Fixture::start(Some(RULES), Some(ALIASES)).await?;
    std::fs::write(f.http.user_aliases_path(), "not: a sequence")?;
    f.assert_session_status("alias-owner", Some("alice"), StatusCode::OK)
        .await?;
    f.assert_session_status("alias-owner", None, StatusCode::UNAUTHORIZED)
        .await?;
    let response = f
        .request(Method::GET, "/v1/agents/allowed", Some(""))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}
