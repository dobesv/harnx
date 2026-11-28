//! End-to-end client-credentials flows against a live token endpoint and a
//! guarded MCP upstream: lazy exchange, wire format, custom headers, rotation
//! without reconnect, single-flight, and the 401 replay policy.

use anyhow::Result;
use rmcp::model::CallToolResult;
use std::sync::atomic::Ordering;

use crate::client_credentials::fixture::{
    echo_args, oauth_args, wait_until_settled, write_client_secret_file, ClientCredentialsUpstream,
    TokenMode, CLIENT_ID, CLIENT_SECRET, CUSTOM_HEADER, CUSTOM_HEADER_VALUE, TOKEN_PREFIX,
};
use crate::common::{spawn_proxy_client_env, text_content};

/// A running client-credentials proxy plus the fixture it talks to. Owns the
/// tempdir holding the secret file, so dropping the bundle cleans up.
struct OAuthProxy {
    service: rmcp::service::RunningService<rmcp::RoleClient, crate::common::TestClientHandler>,
    upstream: ClientCredentialsUpstream,
    _dir: tempfile::TempDir,
}

impl OAuthProxy {
    fn peer(&self) -> rmcp::Peer<rmcp::RoleClient> {
        self.service.peer().clone()
    }

    /// Spawns the secret file and the proxy together — the common setup of
    /// every serving-mode test.
    async fn start(upstream: ClientCredentialsUpstream, extra: &[&str]) -> Result<Self> {
        harnx_core::require_nextest();
        let (dir, secret) = write_client_secret_file();
        let args = oauth_args(&upstream, &secret, extra);
        let (service, _methods, _stderr) = spawn_proxy_client_env(&args, &[]).await?;
        Ok(Self {
            service,
            upstream,
            _dir: dir,
        })
    }

    /// Tears down the downstream stdio session first, then the fixture the
    /// assertions have already read.
    async fn shutdown(self) -> Result<()> {
        self.service.cancel().await?;
        self.upstream.shutdown().await;
        Ok(())
    }
}

fn assert_text(result: &CallToolResult, expected: &str) {
    let text = text_content(result);
    let value: serde_json::Value = serde_json::from_str(&text).expect("echo returns JSON");
    assert_eq!(value["text"], expected, "echo payload: {text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_credentials_mode_proxies_tool_calls_with_oauth_tokens() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let proxy = OAuthProxy::start(upstream, &[]).await?;
    let peer = proxy.peer();

    // The proxy connects upstream at the downstream initialize, which runs
    // one lazy token exchange. The 3600-second validity keeps later requests
    // (notifications/initialized, this call) inside the reuse window.
    assert_eq!(
        proxy.upstream.state.token_exchanges.load(Ordering::SeqCst),
        1
    );

    let result = peer.call_tool(echo_args("hello oauth")).await?;
    assert_text(&result, "hello oauth");
    assert!(!result.is_error.unwrap_or(false));

    // Still exactly one exchange: the initialized notification and the
    // call reuse the cached token. The exchange form is client_credentials
    // with the configured id, the secret from the file, and no scope.
    assert_eq!(
        proxy.upstream.state.token_exchanges.load(Ordering::SeqCst),
        1
    );
    let form = proxy.upstream.state.recorded_token_form(0);
    let get = |name: &str| {
        form.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    assert_eq!(get("grant_type"), "client_credentials");
    assert_eq!(get("client_id"), CLIENT_ID);
    assert_eq!(get("client_secret"), CLIENT_SECRET);
    assert_eq!(get("scope"), "");
    assert_eq!(
        proxy.upstream.state.initialize_count.load(Ordering::SeqCst),
        1
    );

    proxy.shutdown().await
}

/// Scopes from `--oauth-scope` are sent space-joined in the token exchange
/// only; the MCP endpoint sees no scope anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_scopes_are_sent_to_the_token_endpoint_only() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let proxy = OAuthProxy::start(
        upstream,
        &["--oauth-scope", "read", "--oauth-scope", "write"],
    )
    .await?;
    let result = proxy.peer().call_tool(echo_args("scoped")).await?;
    assert_text(&result, "scoped");

    let form = proxy.upstream.state.recorded_token_form(0);
    let scope = form
        .iter()
        .find(|(key, _)| key == "scope")
        .map(|(_, value)| value.clone())
        .expect("scope pair in token form");
    assert_eq!(scope, "read write");
    assert!(!proxy
        .upstream
        .state
        .token_saw_custom_header
        .load(Ordering::SeqCst));

    proxy.shutdown().await
}

/// Unrelated custom headers still reach the MCP upstream, and never the token
/// endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn custom_headers_are_kept_for_mcp_and_absent_from_the_token_exchange() -> Result<()> {
    let upstream =
        ClientCredentialsUpstream::spawn_parts("127.0.0.1:0", TokenMode::Issue, 3600, true).await;
    let header = format!("{CUSTOM_HEADER}:{CUSTOM_HEADER_VALUE}");
    let proxy = OAuthProxy::start(upstream, &["--header", &header]).await?;
    let result = proxy.peer().call_tool(echo_args("with headers")).await?;
    let text = text_content(&result);
    let headers: serde_json::Value = serde_json::from_str(&text).expect("echo returns JSON");
    assert_eq!(
        headers["headers"][CUSTOM_HEADER.to_lowercase()],
        CUSTOM_HEADER_VALUE
    );

    assert!(!proxy
        .upstream
        .state
        .token_saw_custom_header
        .load(Ordering::SeqCst));

    proxy.shutdown().await
}

/// Same-session token rotation: with `expires_in` below the 60-second pre-expiry
/// window every acquired token is refreshed at the next proxied request, and
/// the upstream session is reused — no second initialize for any rotation.
#[tokio::test(flavor = "multi_thread")]
async fn token_rotation_reuses_the_upstream_session() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn_mode(TokenMode::Issue, 2).await;
    let proxy = OAuthProxy::start(upstream, &[]).await?;
    let peer = proxy.peer();

    let first = peer.call_tool(echo_args("call one")).await?;
    assert_text(&first, "call one");

    let second = peer.call_tool(echo_args("call two")).await?;
    assert_text(&second, "call two");
    let state = &proxy.upstream.state;
    wait_until_settled(|| state.token_exchanges.load(Ordering::SeqCst)).await?;

    // Every proxied POST refreshes with a 2-second token: initialize,
    // initialized notification and the two calls. What must NOT happen is a
    // second upstream initialize — rotation is same-session.
    let exchanges = state.token_exchanges.load(Ordering::SeqCst);
    assert!(
        exchanges >= 3,
        "expected at least three rotated exchanges, got {exchanges}"
    );
    assert_eq!(
        state.initialize_count.load(Ordering::SeqCst),
        1,
        "token rotation must not re-initialize the upstream session"
    );
    // Rotation is proactive, not 401-driven.
    assert_eq!(state.unauthorized_401s.load(Ordering::SeqCst), 0);
    // The retired first token was accepted first; later tokens took over.
    assert_eq!(
        state.first_accepted_token.lock().unwrap().clone(),
        Some(format!("{TOKEN_PREFIX}1"))
    );

    proxy.shutdown().await
}

/// Concurrent first calls share one token exchange (single flight).
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_calls_perform_a_single_exchange() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let proxy = OAuthProxy::start(upstream, &[]).await?;
    let peer = proxy.peer();

    let mut calls = Vec::new();
    for index in 0..8 {
        let peer = peer.clone();
        calls.push(tokio::spawn(async move {
            peer.call_tool(echo_args(&format!("parallel {index}")))
                .await
        }));
    }
    for call in calls {
        call.await.expect("join")?;
    }

    let state = &proxy.upstream.state;
    wait_until_settled(|| state.token_exchanges.load(Ordering::SeqCst)).await?;
    assert_eq!(
        state.token_exchanges.load(Ordering::SeqCst),
        1,
        "single flight must collapse concurrent cache misses into one exchange"
    );

    proxy.shutdown().await
}

/// A rejected current token triggers exactly one 401 replay with a fresh
/// token, and the replayed call succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn one_401_replays_once_with_a_fresh_token() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let proxy = OAuthProxy::start(upstream, &[]).await?;
    let peer = proxy.peer();

    let first = peer.call_tool(echo_args("warm up")).await?;
    assert_text(&first, "warm up");
    let state = &proxy.upstream.state;
    assert_eq!(state.token_exchanges.load(Ordering::SeqCst), 1);

    // Retire the live token mid-session, like a provider-side revocation.
    state.reject_token(&format!("{TOKEN_PREFIX}1"));

    let replayed = peer.call_tool(echo_args("after revocation")).await?;
    assert_text(&replayed, "after revocation");
    assert_eq!(
        state.unauthorized_401s.load(Ordering::SeqCst),
        1,
        "exactly one 401"
    );
    assert_eq!(state.token_exchanges.load(Ordering::SeqCst), 2);
    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 1);

    proxy.shutdown().await
}

/// A second 401 stops the call: one replay per presented token, then a safe
/// failure with no token or secret anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn second_401_stops_the_call_with_a_safe_error() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn_mode(TokenMode::Issue, 3600).await;
    upstream
        .state
        .auto_reject_new_tokens
        .store(true, Ordering::SeqCst);
    let message = crate::client_credentials::fixture::rejected_connect_message(
        &upstream,
        &[],
        "a fully rejected token",
    )
    .await?;
    assert!(
        message.contains("client-credentials token refresh failed"),
        "expected the safe refresh-failure text, got: {message}"
    );
    assert!(!message.contains(CLIENT_SECRET), "secret leaked: {message}");
    assert!(
        !message.contains(TOKEN_PREFIX),
        "token value leaked into error: {message}"
    );
    // One 401 for each presented token (initial and replayed), then stop.
    assert_eq!(
        upstream.state.unauthorized_401s.load(Ordering::SeqCst),
        2,
        "exactly one 401 per presented token, then stop"
    );
    assert_eq!(upstream.state.token_exchanges.load(Ordering::SeqCst), 2);
    // The retry policy is bounded: two presented requests, not a retry storm.
    let requests = upstream.state.mcp_requests.load(Ordering::SeqCst);
    assert!(
        (2..=4).contains(&requests),
        "unexpected request storm: {requests} requests"
    );

    upstream.shutdown().await;
    Ok(())
}

/// Non-401 failures are never replayed: a 403 fails the call directly without
/// a second token exchange or a second MCP request.
#[tokio::test(flavor = "multi_thread")]
async fn non_401_failure_is_not_replayed() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let proxy = OAuthProxy::start(upstream, &[]).await?;
    let peer = proxy.peer();

    let ok = peer.call_tool(echo_args("warm up")).await?;
    assert_text(&ok, "warm up");
    let state = &proxy.upstream.state;
    let requests_before = state.mcp_requests.load(Ordering::SeqCst);
    let exchanges_before = state.token_exchanges.load(Ordering::SeqCst);

    state.simulate_403.store(true, Ordering::SeqCst);
    let err = peer
        .call_tool(echo_args("forbidden"))
        .await
        .expect_err("403 must fail the call");
    let message = err.to_string();
    assert!(
        message.contains("insufficient_scope") && message.contains("missing required scope"),
        "expected the safe 403 mapping, got: {message}"
    );
    assert!(!message.contains(CLIENT_SECRET), "secret leaked: {message}");
    assert!(
        !message.contains("Bearer error="),
        "upstream challenge prose leaked: {message}"
    );

    wait_until_settled(|| state.mcp_requests.load(Ordering::SeqCst)).await?;
    assert_eq!(
        state.mcp_requests.load(Ordering::SeqCst),
        requests_before + 1,
        "non-401 must not be replayed"
    );
    assert_eq!(
        state.token_exchanges.load(Ordering::SeqCst),
        exchanges_before,
        "non-401 must not trigger a refresh"
    );
    assert_eq!(state.unauthorized_401s.load(Ordering::SeqCst), 0);

    proxy.shutdown().await
}
