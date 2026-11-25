#![cfg(test)]
//! Integration tests for OAuth client acquisition (DCR, pre-registered, and cross-instance persistence).

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{json, Value};
use url::Url;

use super::*;
use crate::native_common::*;

/// DCR flow completes end-to-end:
/// Starts from unconfigured client, performs DCR on demand, completes browser redirect,
/// persists connection/credentials in NATS, and exercises authenticated MCP tools.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dcr_flow_completes_end_to_end() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    let toolset = fixture.toolset();

    let (challenge_id, round) = start_connect_flow(toolset, "alice").await;

    // Fetch connect page and verify the OAuth relative link is resolved correctly
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let connect_page_url = connect_page_url(&fixture.instance.http_base, &challenge_id);
    let page_res = http.get(&connect_page_url).send().await.expect("page get");
    assert_eq!(page_res.status().as_u16(), 200);
    let html = page_res.text().await.expect("html");
    let rel_link = extract_oauth_link(&html).expect("oauth link in html");
    let base_parsed = Url::parse(&connect_page_url).expect("parse base");
    let resolved_oauth_url = base_parsed.join(&rel_link).expect("url join");
    assert_eq!(
        resolved_oauth_url.as_str(),
        format!(
            "{}/connect/{challenge_id}/oauth",
            fixture.instance.http_base
        )
    );

    // Complete OAuth connect flow
    let (status, body) = drive_oauth_connect(&fixture.instance.http_base, &challenge_id, &[])
        .await
        .expect("drive connect");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Connected. Return to harnx and click Continue."));

    // Verify DCR endpoint was indeed called
    assert_eq!(fixture.mock.state.dcr_count.load(Ordering::SeqCst), 1);

    // Continue in harnx
    let result = continue_connect(toolset, "alice", &challenge_id, round).await;

    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("Connected as alice@example.test; 2 jira tools are available"),
        "{result}"
    );

    // Upstream call reaches mock MCP server with Alice credential
    let who = invoke_tool(toolset, "alice", "whoami")
        .await
        .expect("whoami");
    assert_eq!(who["content"][0]["text"], json!("user:alice@example.test"));

    // Bob cannot call Alice tool
    let bob_who = invoke_tool(toolset, "bob", "whoami")
        .await
        .expect_err("bob denied");
    assert!(matches!(bob_who, ToolInvokeError::Recoverable(_)));

    fixture.shutdown().await;
}

/// Pre-registered client flow completes end-to-end without calling DCR.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preregistered_client_flow_completes() {
    harnx_core::require_nextest();
    let mock = MockOAuthServer::spawn(3600).await.expect("spawn mock");

    let secret_dir = tempfile::TempDir::new().expect("temp dir");
    let secret_path = secret_dir.path().join("oauth_client_secret.txt");
    let generated_secret = generate_secret("prereg-secret");
    std::fs::write(&secret_path, &generated_secret).expect("write secret");

    let extra = [
        "--oauth-client-id",
        "pre-registered-client-123",
        "--oauth-client-secret-file",
        secret_path.to_str().expect("secret path"),
    ];
    let shared = shared_nats(&mock.url, &extra).await;
    let instance = start_instance(&shared).await;
    let toolset = &instance.server.toolset;

    let (challenge_id, round) = start_connect_flow(toolset, "alice").await;

    let (status, body) = drive_oauth_connect(&instance.http_base, &challenge_id, &[])
        .await
        .expect("drive connect");
    assert_eq!(status, 200, "{body}");

    // DCR must NOT have been called
    assert_eq!(mock.state.dcr_count.load(Ordering::SeqCst), 0);

    // Verify token request carried pre-registered client ID and secret without secret leakage in assertion
    {
        let tokens = mock.state.recorded_tokens.lock().unwrap();
        assert!(!tokens.is_empty());
        assert_eq!(
            tokens[0].client_id.as_deref(),
            Some("pre-registered-client-123")
        );
        assert!(
            tokens[0].client_secret.as_deref() == Some(generated_secret.as_str()),
            "client secret matched pre-registered secret"
        );
    }

    let result = continue_connect(toolset, "alice", &challenge_id, round).await;

    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("Connected as alice@example.test; 2 jira tools are available"),
        "{result}"
    );

    let who = invoke(toolset, "whoami", Some("alice"), &[], Value::Null)
        .await
        .expect("whoami");
    assert_eq!(who["content"][0]["text"], json!("user:alice@example.test"));

    instance.shutdown.cancel();
    mock.shutdown().await;
}

/// DCR returning 422 produces a safe configuration error without raw provider leakage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dcr_422_safe_error() {
    harnx_core::require_nextest();
    let fixture = OAuthFixture::spawn(3600).await;
    fixture.mock.state.dcr_422.store(true, Ordering::SeqCst);

    let error = invoke(
        fixture.toolset(),
        "connect",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect_err("connect fails on DCR 422");
    let ToolInvokeError::Recoverable(msg) = error else {
        panic!("expected recoverable error, got {error:?}");
    };

    assert!(
        msg.contains("OAuth dynamic client registration failed; configure --oauth-client-id"),
        "{msg}"
    );

    fixture.shutdown().await;
}

/// Cross-instance flow persistence:
/// Instance A creates the flow and stops. Instance B serves /connect/{id}/oauth and /callback.
/// The original state, PKCE verifier, and resource are successfully recovered from NATS.
// qual:allow(complexity, max_function_lines=150) reason: "multi-stage cross-instance persistence and link resolution test sequence"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_instance_flow_persistence_and_link_resolution() {
    harnx_core::require_nextest();
    let mock = MockOAuthServer::spawn(3600).await.expect("spawn mock");
    let shared = shared_nats(&mock.url, &[]).await;
    let instance_a = start_instance(&shared).await;
    let instance_b = start_instance(&shared).await;

    // Instance A creates the flow
    let (challenge_id, round) = start_connect_flow(&instance_a.server.toolset, "alice").await;

    // Fetch connect page from instance A and verify browser-relative OAuth link resolution using Url::join
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let connect_page_url = connect_page_url(&instance_a.http_base, &challenge_id);
    let page_res = http.get(&connect_page_url).send().await.expect("page get");
    assert_eq!(page_res.status().as_u16(), 200);
    let html = page_res.text().await.expect("html");
    let rel_link = extract_oauth_link(&html).expect("oauth link in html");
    let base_parsed = Url::parse(&connect_page_url).expect("parse base");
    let resolved_oauth_url = base_parsed.join(&rel_link).expect("url join");
    assert_eq!(
        resolved_oauth_url.as_str(),
        format!("{}/connect/{challenge_id}/oauth", instance_a.http_base)
    );

    // Read flow from instance A before stopping it
    let original_flow = instance_a
        .server
        .lifecycle
        .flows
        .get_by_challenge(&challenge_id)
        .await
        .expect("load original durable flow")
        .expect("original durable flow");
    let original_oauth_state = original_flow.record.oauth_state.clone();
    let original_verifier = original_flow.record.pkce_verifier.clone().expect("PKCE");
    let original_registration = original_flow
        .record
        .oauth_client
        .clone()
        .expect("persisted registration");

    // Stop instance A
    instance_a.shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Complete flow through instance B
    let (status, body) = drive_oauth_connect(&instance_b.http_base, &challenge_id, &[])
        .await
        .expect("drive connect on B");
    assert_eq!(status, 200, "{body}");

    // Assert exact persistence/equality of state, PKCE verifier, resource at mock
    {
        let auths = mock.state.recorded_auths.lock().unwrap();
        assert_eq!(auths.len(), 1, "exactly one auth request received");
        let auth = &auths[0];
        assert!(
            auth.state.as_deref() == Some(original_oauth_state.expose().as_str()),
            "instance B must use instance A's persisted OAuth state"
        );
        assert_eq!(
            auth.client_id,
            Some(original_registration.client_id.clone())
        );
        assert_eq!(auth.code_challenge_method.as_deref(), Some("S256"));
        let challenge = auth
            .code_challenge
            .as_ref()
            .expect("code challenge present");
        assert_eq!(
            auth.resource.as_deref(),
            Some(format!("{}/mcp", mock.state.base_url).as_str())
        );

        let tokens = mock.state.recorded_tokens.lock().unwrap();
        assert_eq!(tokens.len(), 1, "exactly one token exchange received");
        let token_req = &tokens[0];
        assert_eq!(token_req.grant_type.as_deref(), Some("authorization_code"));
        assert_eq!(
            token_req.resource.as_deref(),
            Some(format!("{}/mcp", mock.state.base_url).as_str())
        );
        let verifier = token_req
            .code_verifier
            .as_ref()
            .expect("code verifier present");
        assert!(
            verifier == original_verifier.expose(),
            "instance B must exchange with instance A's persisted PKCE verifier"
        );
        assert!(
            token_req.client_secret.as_deref()
                == original_registration
                    .client_secret
                    .as_ref()
                    .map(|value| value.expose().as_str()),
            "instance B must use instance A's persisted client secret"
        );

        // Verify PKCE S256 challenge matches verifier
        let pkce_challenge = oauth2::PkceCodeChallenge::from_code_verifier_sha256(
            &oauth2::PkceCodeVerifier::new(verifier.clone()),
        );
        assert_eq!(
            challenge,
            pkce_challenge.as_str(),
            "PKCE verifier matched challenge"
        );
    }

    // Continue on instance B
    let result = continue_connect(&instance_b.server.toolset, "alice", &challenge_id, round).await;

    assert!(
        result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("Connected as alice@example.test; 2 jira tools are available"),
        "{result}"
    );

    // Upstream call on B works
    let who = invoke(
        &instance_b.server.toolset,
        "whoami",
        Some("alice"),
        &[],
        Value::Null,
    )
    .await
    .expect("whoami on B");
    assert_eq!(who["content"][0]["text"], json!("user:alice@example.test"));

    instance_b.shutdown.cancel();
    mock.shutdown().await;
}
