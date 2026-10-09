//! Group/role grants through HTTP, without changing immutable user ownership.

use anyhow::{Context, Result};
use harnx_blob_store::media::{ensure_attachments_bucket, put_media};
use harnx_core::cid_url::{CidUrl, SessionRef};
use harnx_runtime::nats_session_metadata::{
    session_properties, SessionInitializer, SessionMetadata, SessionMetadataStore,
};
use reqwest::{header::HeaderValue, Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

use crate::{
    access_agents::{Fixture as HttpFixture, MembershipSettings},
    common,
};

const RULES: &str = r#"
rules:
  - agents: [allowed]
    groups: [team, bob]
  - agents: [allowed]
    roles: [prompter]
  - agents: [allowed]
    groups: [admins]
    scopes: [admin]
  - agents: [allowed]
    roles: [auditor]
    scopes: [admin]
  - agents: [allowed]
    users: [user-prompt]
  - agents: [blocked]
    users: [direct]
"#;
const FLAGS: &[&str] = &[
    "--group-header",
    "x-groups",
    "--group-header",
    "x-other-groups",
    "--role-header",
    "x-roles",
];

fn request(f: &HttpFixture, method: Method, path: &str, user: Option<&str>) -> RequestBuilder {
    let builder = f
        .client
        .request(method, format!("{}{path}", f.base))
        .header("accept", "application/json");
    match user {
        Some(user) => builder.header("x-user", user),
        None => builder,
    }
}

async fn visible(builder: RequestBuilder) -> Result<bool> {
    let response = builder.send().await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await?;
    Ok(body["data"]
        .as_array()
        .context("agents")?
        .iter()
        .any(|agent| agent["name"] == "allowed"))
}

#[tokio::test]
async fn membership_discovery_namespaces_and_all_configured_occurrences() -> Result<()> {
    let f = HttpFixture::start_memberships(
        Some(RULES),
        None,
        MembershipSettings {
            args: FLAGS,
            ..Default::default()
        },
    )
    .await?;
    for (user, header, value, expected) in [
        ("alice", "x-groups", "team", true),
        ("alice", "x-roles", "prompter", true),
        ("alice", "x-groups", "admins", true),
        ("alice", "x-roles", "auditor", true),
        ("team", "x-roles", "team", false),
        ("auditor", "x-groups", "auditor", false),
        ("alice", "x-unconfigured", "team", false),
        ("alice", "x-groups", " , ", false),
    ] {
        assert_eq!(
            visible(request(&f, Method::GET, "/v1/agents", Some(user)).header(header, value))
                .await?,
            expected,
            "{user} {header}"
        );
    }
    let defaults =
        HttpFixture::start_memberships(Some(RULES), None, MembershipSettings::default()).await?;
    assert!(
        !visible(
            request(&defaults, Method::GET, "/v1/agents", Some("alice")).header("x-groups", "team")
        )
        .await?
    );
    let repeated = request(&f, Method::GET, "/v1/agents", Some("alice"))
        .header("x-groups", " , ungranted,")
        .header("x-groups", "ungranted-again")
        .header("x-other-groups", " , team, team, ");
    assert!(visible(repeated).await?);
    let response = f
        .client
        .get(format!("{}/v1/agents/allowed", f.base))
        .header("x-user", "alice")
        .header("x-roles", "prompter")
        .header("accept", "text/html")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn membership_malformed_later_values_fail_closed_without_echo_and_user_is_required(
) -> Result<()> {
    let f = HttpFixture::start_memberships(
        Some(RULES),
        None,
        MembershipSettings {
            args: FLAGS,
            ..Default::default()
        },
    )
    .await?;
    for header in ["x-groups", "x-other-groups", "x-roles"] {
        let response = request(&f, Method::GET, "/v1/agents", Some("alice"))
            .header("x-groups", "team")
            .header("x-roles", "prompter")
            .header(header, HeaderValue::from_bytes(b"sensitive-\xff").unwrap())
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = response.text().await?;
        assert!(body.contains("invalid user identity source"));
        assert!(!body.contains("sensitive"));
        assert!(!body.contains("prompter"));
    }
    for path in [
        "/v1/agents",
        "/v1/agents/allowed",
        "/v1/agents/allowed/sessions",
        "/v1/cid/invalid",
    ] {
        let response = request(&f, Method::GET, path, None)
            .header("x-groups", "admins")
            .header("x-roles", "prompter")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
    assert!(
        visible(
            request(&f, Method::GET, "/v1/agents", Some("alice"))
                .header("x-groups", "team")
                .header("x-spoof", HeaderValue::from_bytes(b"\xff").unwrap())
        )
        .await?
    );
    let options = request(&f, Method::OPTIONS, "/v1/agents", None)
        .header("x-groups", HeaderValue::from_bytes(b"\xff").unwrap())
        .send()
        .await?;
    assert_eq!(options.status(), StatusCode::NO_CONTENT);
    Ok(())
}

fn precedence_cases() -> [(MembershipSettings<'static>, Vec<&'static str>); 4] {
    let yaml = "serve_group_headers: [x-yaml]\nserve_role_headers: [r-yaml]\n";
    let env = &[
        ("HARNX_SERVE_GROUP_HEADERS", "x-env"),
        ("HARNX_SERVE_ROLE_HEADERS", "r-env"),
    ];
    let args = &[
        "--group-header",
        "x-cli",
        "--group-header",
        "x-cli2",
        "--role-header",
        "r-cli",
        "--role-header",
        "r-cli2",
    ];
    [
        (
            MembershipSettings {
                config: yaml,
                ..Default::default()
            },
            vec!["x-yaml", "r-yaml"],
        ),
        (
            MembershipSettings {
                config: yaml,
                env,
                ..Default::default()
            },
            vec!["x-env", "r-env"],
        ),
        (
            MembershipSettings {
                config: yaml,
                env,
                args,
            },
            vec!["x-cli", "x-cli2", "r-cli", "r-cli2"],
        ),
        (
            MembershipSettings {
                config: yaml,
                env: &[
                    ("HARNX_SERVE_GROUP_HEADERS", " , "),
                    ("HARNX_SERVE_ROLE_HEADERS", ""),
                ],
                ..Default::default()
            },
            vec![],
        ),
    ]
}

#[tokio::test]
async fn membership_config_env_cli_precedence_and_repeatable_flags() -> Result<()> {
    for (settings, trusted) in precedence_cases() {
        let f = HttpFixture::start_memberships(Some(RULES), None, settings).await?;
        for name in [
            "x-yaml", "x-env", "x-cli", "x-cli2", "r-yaml", "r-env", "r-cli", "r-cli2",
        ] {
            let value = if name.starts_with('x') {
                "team"
            } else {
                "prompter"
            };
            assert_eq!(
                visible(request(&f, Method::GET, "/v1/agents", Some("alice")).header(name, value))
                    .await?,
                trusted.contains(&name),
                "{name}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn membership_invalid_startup_names_fail_even_with_rules_disabled() -> Result<()> {
    for (settings, category) in [
        (
            MembershipSettings {
                config: "serve_group_headers: ['invalid name']\n",
                ..Default::default()
            },
            "serve_group_headers",
        ),
        (
            MembershipSettings {
                config: "serve_role_headers: ['cookie:roles']\n",
                ..Default::default()
            },
            "serve_role_headers",
        ),
        (
            MembershipSettings {
                env: &[("HARNX_SERVE_GROUP_HEADERS", "header:groups")],
                ..Default::default()
            },
            "serve_group_headers",
        ),
        (
            MembershipSettings {
                args: &["--role-header", "invalid name"],
                ..Default::default()
            },
            "serve_role_headers",
        ),
    ] {
        let result = HttpFixture::start_memberships(None, None, settings).await;
        let error = result
            .err()
            .context("invalid configuration must fail startup")?;
        assert!(error.to_string().contains(category), "{error}");
    }
    Ok(())
}

#[tokio::test]
async fn membership_rules_disabled_preserve_anonymous_behavior_and_ignore_unconfigured_headers(
) -> Result<()> {
    let f = HttpFixture::start_memberships(None, None, MembershipSettings::default()).await?;
    assert!(
        visible(
            request(&f, Method::GET, "/v1/agents", None)
                .header("x-groups", HeaderValue::from_bytes(b"\xff").unwrap())
        )
        .await?
    );
    let configured = HttpFixture::start_memberships(
        None,
        None,
        MembershipSettings {
            args: FLAGS,
            ..Default::default()
        },
    )
    .await?;
    assert!(
        visible(request(&configured, Method::GET, "/v1/agents", None).header("x-groups", "team"))
            .await?
    );
    Ok(())
}

struct Fixture {
    http: HttpFixture,
    store: SessionMetadataStore,
    cid: CidUrl,
    _nats: common::NatsServerHandle,
}

async fn fixture() -> Result<Option<Fixture>> {
    harnx_core::require_nextest();
    let Some(nats) = common::spawn_nats_server().await? else {
        return Ok(None);
    };
    let jetstream = async_nats::jetstream::new(async_nats::connect(nats.url()).await?);
    let store = SessionMetadataStore::ensure(&jetstream, 1).await?;
    for (id, owner) in [
        ("owned", Some("alice")),
        ("foreign", Some("bob")),
        ("legacy", None),
    ] {
        let mut initializer = SessionInitializer::named("allowed", Default::default());
        if let Some(owner) = owner {
            initializer = initializer.with_user_id(owner);
        }
        store.create(&SessionMetadata::new(id, initializer)).await?;
    }
    let cid = CidUrl::Media {
        session: SessionRef::new(Some("allowed".into()), "foreign".into())?,
        hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
    };
    put_media(
        &ensure_attachments_bucket(&jetstream, 1).await?,
        &cid,
        b"private image",
        "image/png",
    )
    .await?;
    let http = HttpFixture::start_memberships(
        Some(RULES),
        Some(nats.url()),
        MembershipSettings {
            args: FLAGS,
            ..Default::default()
        },
    )
    .await?;
    Ok(Some(Fixture {
        http,
        store,
        cid,
        _nats: nats,
    }))
}

impl Fixture {
    fn request(&self, method: Method, path: &str, grant: (&str, &str)) -> RequestBuilder {
        request(&self.http, method, path, Some("alice")).header(grant.0, grant.1)
    }

    async fn rpc(&self, id: &str, grant: (&str, &str), method: &str) -> Result<Value> {
        let response = self
            .request(
                Method::POST,
                &format!("/v1/agents/allowed/sessions/{id}"),
                grant,
            )
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":{"text":"hello"}}))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        let body: Value = response.json().await?;
        assert!(body.get("result").is_some(), "{body}");
        Ok(body)
    }
}

#[tokio::test]
async fn membership_nats_group_and_role_prompt_grants_create_read_prompt_and_revoke() -> Result<()>
{
    let Some(f) = fixture().await? else {
        return Ok(());
    };
    for grant in [("x-groups", "team"), ("x-roles", "prompter")] {
        let response = f
            .request(Method::POST, "/v1/agents/allowed/sessions", grant)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await?;
        let id = body["session_id"].as_str().context("created session")?;
        f.rpc(id, grant, "session/get").await?;
        f.rpc(id, grant, "session/prompt").await?;
        let path = format!("/v1/agents/allowed/sessions/{id}/metadata");
        assert_eq!(
            f.request(Method::GET, &path, grant).send().await?.status(),
            StatusCode::OK
        );
        // Same user and session, without a membership on the next request.
        assert_eq!(
            request(&f.http, Method::GET, &path, Some("alice"))
                .send()
                .await?
                .status(),
            StatusCode::NOT_FOUND
        );
        let record = f
            .store
            .get_for_agent(id, "allowed")
            .await?
            .context("metadata")?;
        let properties = session_properties(&record.metadata)?;
        assert_eq!(properties.text("user_id"), Some("alice"));
        assert_eq!(properties.text("groups"), None);
        assert_eq!(properties.text("roles"), None);
    }
    Ok(())
}

#[tokio::test]
async fn membership_nats_prompt_never_grants_foreign_or_group_ownership() -> Result<()> {
    let Some(f) = fixture().await? else {
        return Ok(());
    };
    for grant in [
        ("x-groups", "team"),
        ("x-roles", "prompter"),
        ("x-groups", "bob"),
    ] {
        for id in ["foreign", "legacy"] {
            for suffix in ["", "/metadata", "/usage", "/events", "/attachments/invalid"] {
                let path = format!("/v1/agents/allowed/sessions/{id}{suffix}");
                assert_eq!(
                    f.request(Method::GET, &path, grant).send().await?.status(),
                    StatusCode::NOT_FOUND,
                    "{path}"
                );
            }
            let response = f.request(Method::POST, &format!("/v1/agents/allowed/sessions/{id}"), grant)
                .json(&json!({"jsonrpc":"2.0", "id":1, "method":"session/prompt", "params":{"text":"hello"}})).send().await?;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let response = f
            .request(Method::GET, "/v1/agents/allowed/sessions?limit=100", grant)
            .send()
            .await?;
        let body: Value = response.json().await?;
        assert_eq!(
            body["sessions"].as_array().context("sessions")?.len(),
            1,
            "{body}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn membership_nats_admin_union_lists_foreign_reads_and_preserves_owner() -> Result<()> {
    let Some(f) = fixture().await? else {
        return Ok(());
    };
    for grant in [("x-groups", "admins"), ("x-roles", "auditor")] {
        let response = f
            .request(Method::POST, "/v1/agents/allowed/sessions", grant)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        f.rpc("foreign", grant, "session/get").await?;
        let response = f
            .request(Method::GET, "/v1/agents/allowed/sessions?limit=100", grant)
            .send()
            .await?;
        let body: Value = response.json().await?;
        assert_eq!(
            body["sessions"].as_array().context("sessions")?.len(),
            3,
            "{body}"
        );
        let agent: Value = f
            .request(Method::GET, "/v1/agents/allowed", grant)
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(
            agent["sessions"]
                .as_array()
                .context("agent sessions")?
                .len(),
            3
        );
    }
    let response = f
        .request(
            Method::POST,
            "/v1/agents/allowed/sessions",
            ("x-groups", "team"),
        )
        .header("x-roles", "auditor")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await?;
    let id = body["session_id"].as_str().context("created")?;
    f.rpc("foreign", ("x-roles", "auditor"), "session/prompt")
        .await?;
    let foreign = f
        .store
        .get_for_agent("foreign", "allowed")
        .await?
        .context("foreign")?;
    assert_eq!(
        session_properties(&foreign.metadata)?.text("user_id"),
        Some("bob")
    );
    let own = f.store.get_for_agent(id, "allowed").await?.context("new")?;
    assert_eq!(
        session_properties(&own.metadata)?.text("user_id"),
        Some("alice")
    );
    Ok(())
}

#[tokio::test]
async fn membership_nats_cid_admin_access_is_private_and_revoked_on_next_request() -> Result<()> {
    let Some(f) = fixture().await? else {
        return Ok(());
    };
    let path = format!(
        "/v1/cid/{}",
        percent_encoding::utf8_percent_encode(
            &f.cid.to_string(),
            percent_encoding::NON_ALPHANUMERIC
        )
    );
    for grant in [("x-groups", "admins"), ("x-roles", "auditor")] {
        let response = f.request(Method::GET, &path, grant).send().await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        assert_eq!(response.bytes().await?.as_ref(), b"private image");
        let response = request(&f.http, Method::GET, &path, Some("alice"))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
    }
    let response = f
        .request(Method::GET, &path, ("x-groups", "team"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn membership_nats_user_group_role_scopes_are_unioned() -> Result<()> {
    let Some(f) = fixture().await? else {
        return Ok(());
    };
    let response = request(
        &f.http,
        Method::POST,
        "/v1/agents/allowed/sessions",
        Some("user-prompt"),
    )
    .header("x-groups", "admins")
    .header("x-roles", "prompter")
    .send()
    .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await?;
    let id = body["session_id"].as_str().context("created session")?;
    let record = f
        .store
        .get_for_agent(id, "allowed")
        .await?
        .context("metadata")?;
    assert_eq!(
        session_properties(&record.metadata)?.text("user_id"),
        Some("user-prompt")
    );
    let response = request(
        &f.http,
        Method::POST,
        "/v1/agents/allowed/sessions/foreign",
        Some("user-prompt"),
    )
    .header("x-groups", "admins")
    .header("x-roles", "prompter")
    .json(&json!({"jsonrpc":"2.0", "id":1, "method":"session/get"}))
    .send()
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await?;
    assert!(body.get("result").is_some(), "{body}");
    Ok(())
}
