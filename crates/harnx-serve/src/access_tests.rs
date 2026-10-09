use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use harnx_core::access_rules::AccessRules;
use harnx_runtime::config::{ConfigLock, GlobalConfig};
use reqwest::{Client, Method, StatusCode};
use serde_json::Value;

use crate::{request_identity::RequestIdentity, test_support::TestConfigSandbox, Server};

fn access_rules() -> Arc<AccessRules> {
    Arc::new(AccessRules::from_yaml("rules:\n  - agents: ['*']\n    users: [alice]\n").unwrap())
}

fn config(sandbox: &TestConfigSandbox, sources: &[&str]) -> GlobalConfig {
    harnx_core::require_nextest();
    let mut config = sandbox.config();
    config.serve_user_id_sources = sources.iter().map(|s| s.to_string()).collect();
    // A configured session-owner default must never substitute for caller identity.
    config.user_id = Some("alice".to_string());
    Arc::new(ConfigLock::new(config))
}

#[test]
fn access_rules_require_identity_sources_at_construction() {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox, &[]);
    let error =
        Server::new_with_access_rules(&config, PathBuf::from("web-assets"), Some(access_rules()))
            .err()
            .expect("rules without request identity sources must fail");
    let message = error.to_string();
    for expected in [
        "access rules require request identity sources",
        "--user-id-source",
        "HARNX_SERVE_USER_ID_SOURCES",
        "serve_user_id_sources",
    ] {
        assert!(message.contains(expected), "{message}");
    }
    assert!(Server::new_with_access_rules(&config, PathBuf::from("web-assets"), None).is_ok());
}

#[tokio::test]
async fn access_default_file_rejects_startup_without_identity_sources() -> Result<()> {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox, &[]);
    std::fs::write(
        harnx_core::config_paths::local_path("access.yaml"),
        "rules: []\n",
    )?;
    // An invalid bind address proves validation happens before opening the API listener.
    let error = crate::run(config, Some("invalid-address".into()), None, None)
        .await
        .expect_err("startup must reject rules without identity sources");
    assert!(error
        .to_string()
        .contains("access rules require request identity sources"));
    Ok(())
}

#[test]
fn access_helper_uses_request_identity_and_keeps_empty_identity_set_enabled() {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox, &["x-user"]);
    let rules = access_rules();
    let server =
        Server::new_with_access_rules(&config, PathBuf::from("web-assets"), Some(rules.clone()))
            .unwrap();
    let mut req = hyper::Request::builder()
        .header("x-user", "unresolved-header")
        .body(())
        .unwrap();
    let (_, identities) = server.access(&req).expect("checks remain enabled");
    assert!(identities.user_id.is_none());
    req.extensions_mut().insert(RequestIdentity {
        user_id: Some("alice".into()),
        ..Default::default()
    });
    let (actual_rules, identities) = server.access(&req).unwrap();
    // Extracting context must not retain a request borrow: routes consume requests.
    drop(req);
    assert!(std::ptr::eq(actual_rules, rules.as_ref()));
    assert_eq!(identities.user_id.as_deref(), Some("alice"));
    assert!(actual_rules.can_see_agent("plain", identities.caller().view()));
    let disabled = Server::new(&config, PathBuf::from("web-assets"));
    assert!(disabled.access(&hyper::Request::new(())).is_none());
}

async fn serve(server: Server) -> Result<(String, crate::ServerHandle)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let handle = Arc::new(server)
        .run(listener, Duration::from_secs(5))
        .await?;
    Ok((base, handle))
}

#[tokio::test]
async fn access_missing_identity_rejects_agent_and_cid_routes() -> Result<()> {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox, &["x-user"]);
    let (base, handle) = serve(Server::new_with_access_rules(
        &config,
        PathBuf::from("web-assets"),
        Some(access_rules()),
    )?)
    .await?;
    let client = Client::new();
    for (method, path) in [
        (Method::GET, "/v1/agents"),
        (Method::GET, "/v1/agents/"),
        (Method::GET, "/v1/agents/plain"),
        (Method::POST, "/v1/agents/plain/sessions"),
        (Method::GET, "/v1/agents/plain/sessions/test/metadata"),
        (Method::POST, "/v1/agents/plain/sessions/test/attachments"),
        (Method::GET, "/v1/cid/not-a-cid"),
        (Method::GET, "/v1/cid/"),
    ] {
        let response = client
            .request(method, format!("{base}{path}"))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        let body: Value = response.json().await?;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "missing user identity");
    }
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn access_identity_passes_boundary_and_public_routes_remain_open() -> Result<()> {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox, &["x-user", "cookie:owner"]);
    let assets = tempfile::tempdir()?;
    std::fs::write(assets.path().join("index.html"), "public shell")?;
    let (base, handle) = serve(Server::new_with_access_rules(
        &config,
        assets.path().to_path_buf(),
        Some(access_rules()),
    )?)
    .await?;
    let client = Client::new();
    for (path, status) in [
        ("/v1/agents", StatusCode::OK),
        ("/v1/cid/not-a-cid", StatusCode::BAD_REQUEST),
    ] {
        for (header, identity) in [("x-user", "alice"), ("cookie", "owner=alice")] {
            let response = client
                .get(format!("{base}{path}"))
                .header(header, identity)
                .send()
                .await?;
            assert_eq!(response.status(), status, "{header} on {path}");
        }
    }
    for (path, status) in [
        ("/v1/models", StatusCode::OK),
        ("/", StatusCode::OK),
        ("/healthz", StatusCode::NOT_FOUND),
        ("/v1/agents-other", StatusCode::NOT_FOUND),
        ("/v1/cid-other", StatusCode::NOT_FOUND),
    ] {
        let response = client.get(format!("{base}{path}")).send().await?;
        assert_eq!(response.status(), status, "{path}");
    }
    let shell = client
        .get(format!("{base}/agents/plain/sessions/test"))
        .header("accept", "text/html")
        .send()
        .await?;
    assert_eq!(shell.status(), StatusCode::OK);
    assert_public_preflight(&client, &base).await?;
    // Health checks use a separate listener, outside API identity resolution.
    let health_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let health_addr = health_listener.local_addr()?;
    drop(health_listener);
    let readiness = harnx_healthz::init(&harnx_healthz::HealthzFlags {
        healthz_addr: Some(health_addr.to_string()),
    })
    .await?
    .unwrap();
    readiness.ready();
    let response = client
        .get(format!("http://{health_addr}/healthz"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    handle.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn access_disabled_preserves_missing_identity_behavior() -> Result<()> {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox, &["x-user"]);
    let (base, handle) = serve(Server::new(&config, PathBuf::from("web-assets"))).await?;
    let client = Client::new();
    for (path, status) in [
        ("/v1/agents", StatusCode::OK),
        ("/v1/cid/not-a-cid", StatusCode::BAD_REQUEST),
    ] {
        let response = client.get(format!("{base}{path}")).send().await?;
        assert_eq!(response.status(), status, "{path}");
    }
    // Rules don't change the existing fail-closed handling of malformed identities.
    let response = client
        .get(format!("{base}/v1/agents"))
        .header("x-user", "")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    handle.shutdown().await?;
    Ok(())
}

async fn assert_public_preflight(client: &Client, base: &str) -> Result<()> {
    for path in ["/v1/agents", "/v1/cid/not-a-cid", "/v1/models"] {
        for malformed in [false, true] {
            let mut request = client.request(Method::OPTIONS, format!("{base}{path}"));
            if malformed {
                request = request.header("x-user", "");
            }
            let response = request.send().await?;
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
            assert_eq!(response.headers()["access-control-allow-origin"], "*");
        }
    }
    Ok(())
}
