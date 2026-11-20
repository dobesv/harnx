//! Caller-principals snapshot through the real HTTP server and isolated NATS.

use crate::{
    access_agents::{Fixture as HttpFixture, MembershipSettings},
    common,
};
use anyhow::{Context, Result};
use harnx_core::user_principals::{from_resolved, legacy_fallback, UserPrincipals};
use harnx_runtime::nats_session_metadata::{
    user_principals, SessionMetadata, SessionMetadataStore, SESSION_PROPERTIES_NAMESPACE,
    USER_PRINCIPALS_PROPERTY,
};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

const ALIASES: &str =
    "- name: Team\n  identities: [alice, bob]\n  groups: [engineering]\n  roles: [reviewer]\n";

struct Fixture {
    http: HttpFixture,
    store: SessionMetadataStore,
    _nats: common::NatsServerHandle,
}

impl Fixture {
    async fn start() -> Result<Self> {
        harnx_core::require_nextest();
        let nats = common::spawn_nats_server()
            .await?
            .context("NATS required for principals integration")?;
        let client = async_nats::connect(nats.url()).await?;
        let store = SessionMetadataStore::ensure(&async_nats::jetstream::new(client), 1).await?;
        let http = HttpFixture::start_with_user_aliases(
            None,
            Some(nats.url()),
            MembershipSettings {
                config: "serve_group_headers: [x-groups]\nserve_role_headers: [x-roles]\n",
                ..Default::default()
            },
            Some(ALIASES),
        )
        .await?;
        Ok(Self {
            http,
            store,
            _nats: nats,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .client
            .request(method, format!("{}{path}", self.http.base))
            .header("accept", "application/json")
            .header("x-user", "alice")
            .header("x-groups", "ops")
    }

    async fn create_session(&self) -> Result<String> {
        let response = self
            .request(Method::POST, "/v1/agents/allowed/sessions")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await?;
        Ok(body["session_id"]
            .as_str()
            .context("created session id")?
            .to_owned())
    }

    async fn principals(&self, id: &str) -> Result<UserPrincipals> {
        let record = self
            .store
            .get_for_agent(id, "allowed")
            .await?
            .context("canonical session")?;
        Ok(user_principals(&record.metadata))
    }

    /// Header memberships resolve first, alias memberships follow, resolver
    /// order preserved: the exact shape D1 requires.
    fn resolved_principals() -> UserPrincipals {
        from_resolved(
            Some("alice"),
            Some(&["alice".into(), "bob".into()]),
            &["ops".into(), "engineering".into()],
            &["reviewer".into()],
        )
    }
}

#[tokio::test]
async fn session_create_stamps_resolved_principals() -> Result<()> {
    let f = Fixture::start().await?;
    let id = f.create_session().await?;
    assert_eq!(
        f.principals(&id).await?,
        Fixture::resolved_principals(),
        "exact resolver order must survive storage"
    );
    // The snapshot is read-only and inherits: stored once, flagged once.
    let record = f
        .store
        .get_for_agent(&id, "allowed")
        .await?
        .context("canonical session")?;
    let properties = harnx_runtime::nats_session_metadata::session_properties(&record.metadata)?;
    let property = properties
        .get(USER_PRINCIPALS_PROPERTY)
        .context("property")?;
    assert!(property.inherit);
    assert_eq!(
        serde_json::to_value(&record.metadata.extensions)?[SESSION_PROPERTIES_NAMESPACE]
            [USER_PRINCIPALS_PROPERTY],
        json!({
            "value": {
                "user_id": "alice",
                "users": ["alice", "bob"],
                "groups": ["ops", "engineering"],
                "roles": ["reviewer"],
            },
            "inherit": true,
        })
    );
    Ok(())
}

#[tokio::test]
async fn prompt_created_session_stamps_request_principals() -> Result<()> {
    let f = Fixture::start().await?;
    let id = "principals-via-prompt";
    let response = f
        .request(
            Method::POST,
            &format!("/v1/agents/allowed/sessions/{id}"),
        )
        .json(&json!({"jsonrpc":"2.0", "id":1, "method":"session/prompt", "params":{"text":"create through prompt"}}))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(f.principals(id).await?, Fixture::resolved_principals());
    Ok(())
}

#[tokio::test]
async fn clients_cannot_set_or_change_the_snapshot() -> Result<()> {
    let f = Fixture::start().await?;
    let id = f.create_session().await?;
    let before = f.principals(&id).await?;

    // The properties namespace is reserved: PUT and DELETE are refused.
    let namespace = SESSION_PROPERTIES_NAMESPACE.replace('.', "%2E");
    for method in [Method::PUT, Method::DELETE] {
        let response = f
            .request(
                method,
                &format!("/v1/agents/allowed/sessions/{id}/metadata/extensions/{namespace}"),
            )
            .json(&json!({
                USER_PRINCIPALS_PROPERTY: {"value": {"user_id": "mallory", "users": ["mallory"], "groups": ["admins"], "roles": []}, "inherit": true}
            }))
            .send()
            .await?;
        assert!(
            !response.status().is_success(),
            "{} must be refused: {}",
            response.status(),
            response.text().await?
        );
    }

    // A metadata patch cannot name the property: unknown fields are rejected.
    let response = f
        .request(Method::PATCH, &format!("/v1/agents/allowed/sessions/{id}/metadata"))
        .json(&json!({
            USER_PRINCIPALS_PROPERTY: {"value": {"user_id": "mallory", "users": ["mallory"], "groups": [], "roles": []}}
        }))
        .send()
        .await?;
    assert!(
        !response.status().is_success(),
        "{} must be refused: {}",
        response.status(),
        response.text().await?
    );

    assert_eq!(f.principals(&id).await?, before, "snapshot is unchanged");
    Ok(())
}

#[tokio::test]
async fn older_sessions_load_and_resolve_the_legacy_fallback() -> Result<()> {
    let f = Fixture::start().await?;
    // A record written before snapshots existed: owner id only.
    let initializer = harnx_runtime::SessionInitializer::named("allowed", Default::default())
        .with_user_id("legacy-owner");
    f.store
        .create(&SessionMetadata::new("legacy", initializer))
        .await?
        .context("legacy fixture session")?;
    assert_eq!(
        f.principals("legacy").await?,
        legacy_fallback(Some("legacy-owner"))
    );

    // Anonymous callers never stamp: no property, anonymous read.
    let anonymous = f.create_session_anonymous().await?;
    let record = f
        .store
        .get_for_agent(&anonymous, "allowed")
        .await?
        .context("canonical session")?;
    assert!(
        harnx_runtime::nats_session_metadata::session_properties(&record.metadata)?
            .get(USER_PRINCIPALS_PROPERTY)
            .is_none()
    );
    assert_eq!(
        f.principals(&anonymous).await?,
        harnx_core::user_principals::anonymous()
    );
    Ok(())
}

impl Fixture {
    async fn create_session_anonymous(&self) -> Result<String> {
        let response = self
            .http
            .client
            .request(
                Method::POST,
                format!("{}/v1/agents/allowed/sessions", self.http.base),
            )
            .header("accept", "application/json")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await?;
        Ok(body["session_id"]
            .as_str()
            .context("created session id")?
            .to_owned())
    }
}

#[tokio::test]
async fn ag_ui_sse_run_stamps_request_principals_on_session_creation() -> Result<()> {
    let f = Fixture::start().await?;
    let session_id = "ag-ui-principals";
    let response = f
        .http
        .client
        .post(format!(
            "{}/v1/agents/allowed/sessions/{session_id}",
            f.http.base
        ))
        .header("accept", "text/event-stream")
        .header("x-user", "alice")
        .header("x-groups", "ops")
        .header("x-roles", "incident-commander")
        .json(&json!({
            "threadId": uuid::Uuid::new_v4(),
            "runId": uuid::Uuid::new_v4(),
            "messages": [{
                "id": uuid::Uuid::new_v4(),
                "role": "user",
                "content": "start this AG-UI run"
            }]
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    assert_eq!(
        f.principals(session_id).await?,
        from_resolved(
            Some("alice"),
            Some(&["alice".into(), "bob".into()]),
            &["ops".into(), "engineering".into()],
            &["incident-commander".into(), "reviewer".into()],
        ),
        "AG-UI run route must stamp principal snapshot before session execution"
    );
    Ok(())
}
