//! Secrecy of the credential boundary: every upstream failure surface —
//! JSON-RPC errors, `is_error` tool results, errors carrying retired tokens,
//! and refused redirects — must reach the downstream client as static safe
//! text with the secret, tokens and URLs removed, while successful tool
//! output passes through untouched.

use anyhow::Result;
use std::sync::atomic::Ordering;

use crate::client_credentials::fixture::{
    call_tool_args, echo_args, oauth_args, wait_until_settled, write_client_secret_file,
    ClientCredentialsUpstream, TokenMode, CLIENT_SECRET, INIT_DATA_SENTINEL, INIT_MSG_SENTINEL,
    TOKEN_PREFIX,
};
use crate::common::{
    spawn_proxy_client_env, spawn_proxy_client_rejected, stderr_wait_eof, text_content,
};

use super::boundaries::HOSTILE_LOGGING;
use super::fixture::SAFE_UPSTREAM_FAILURE;

/// Secrets that must never appear on any downstream surface.
const SECRETS: &[&str] = &[CLIENT_SECRET];

fn assert_sanitized(label: &str, text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "{label} leaked secret: {text}");
    }
    assert!(
        !text.contains(TOKEN_PREFIX),
        "{label} leaked a token value: {text}"
    );
}

async fn start(
    upstream: &ClientCredentialsUpstream,
    secret_file: &std::path::Path,
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, crate::common::TestClientHandler>> {
    harnx_core::require_nextest();
    let args = oauth_args(upstream, secret_file, &[]);
    let (proxy, _methods, _stderr) = spawn_proxy_client_env(&args, &[]).await?;
    Ok(proxy)
}

/// A JSON-RPC service error from upstream carries untrusted prose (here it
/// echoes the presented token). The code survives; the message does not.
#[tokio::test(flavor = "multi_thread")]
async fn upstream_jsonrpc_error_is_replaced_with_safe_text() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let proxy = start(&upstream, &secret).await?;
    let peer = proxy.peer().clone();

    peer.call_tool(echo_args("warm up")).await?; // first token now accepted

    let err = peer
        .call_tool(call_tool_args("fail_jsonrpc"))
        .await
        .expect_err("fail_jsonrpc must surface as an error");
    let rmcp::service::ServiceError::McpError(err) = err else {
        panic!("upstream JSON-RPC error must arrive as McpError, got: {err}");
    };
    assert_eq!(err.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    let message = err.to_string();
    assert!(
        message.contains(SAFE_UPSTREAM_FAILURE),
        "expected safe replacement text, got: {message}"
    );
    assert_sanitized("JSON-RPC error", &message);

    proxy.cancel().await?;
    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

/// An `is_error: true` tool result is an Ok service response, so it needs its
/// own substitution. The flag and envelope survive; the content does not.
#[tokio::test(flavor = "multi_thread")]
async fn upstream_is_error_tool_result_is_sanitized() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let proxy = start(&upstream, &secret).await?;
    let peer = proxy.peer().clone();

    peer.call_tool(echo_args("warm up")).await?;

    let result = peer.call_tool(call_tool_args("fail_is_error")).await?;
    assert_eq!(result.is_error, Some(true), "failure flag must survive");
    let text = text_content(&result);
    assert_eq!(text, SAFE_UPSTREAM_FAILURE);
    assert_sanitized("is_error tool result", &text);

    proxy.cancel().await?;
    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

/// Rotation retires a token that may still be echoed by an in-flight upstream
/// error. The safe replacement must cover the retired token too.
#[tokio::test(flavor = "multi_thread")]
async fn retired_token_in_upstream_error_is_sanitized() -> Result<()> {
    // expires_in=2 keeps every acquire inside the pre-expiry refresh window,
    // so the second call rotates to token-2 and retires token-1.
    let upstream = ClientCredentialsUpstream::spawn_mode(TokenMode::Issue, 2).await;
    let (dir, secret) = write_client_secret_file();
    let proxy = start(&upstream, &secret).await?;
    let peer = proxy.peer().clone();

    peer.call_tool(echo_args("call one")).await?;
    let second = peer.call_tool(echo_args("call two")).await?;
    let _ = second;
    wait_until_settled(|| upstream.state.token_exchanges.load(Ordering::SeqCst)).await?;
    assert!(
        upstream.state.token_exchanges.load(Ordering::SeqCst) >= 2,
        "rotation must retire token-1"
    );

    let err = peer
        .call_tool(call_tool_args("fail_with_retired_token"))
        .await
        .expect_err("fail_with_retired_token must surface as an error");
    let message = err.to_string();
    assert!(
        message.contains(SAFE_UPSTREAM_FAILURE),
        "expected safe replacement text, got: {message}"
    );
    // The upstream tool specifically echoes the retired token-1; it must not
    // survive the boundary.
    assert!(
        !message.contains(&format!("{TOKEN_PREFIX}1")),
        "retired token leaked: {message}"
    );
    assert_sanitized("retired-token error", &message);

    proxy.cancel().await?;
    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

/// Successful tool output is untouched in client-credentials mode.
#[tokio::test(flavor = "multi_thread")]
async fn successful_tool_output_passes_through_unchanged() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let proxy = start(&upstream, &secret).await?;
    let peer = proxy.peer().clone();

    for text in ["first", "second", "third"] {
        let result = peer.call_tool(echo_args(text)).await?;
        assert_ne!(result.is_error, Some(true), "call must succeed");
        let value: serde_json::Value =
            serde_json::from_str(&text_content(&result)).expect("echo JSON");
        assert_eq!(value["text"], text);
    }
    // One exchange served all three calls.
    assert_eq!(upstream.state.token_exchanges.load(Ordering::SeqCst), 1);

    proxy.cancel().await?;
    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

/// The leak target records POSTs, which is the method preserved by the
/// fixture's temporary (307) redirects.
#[tokio::test(flavor = "multi_thread")]
async fn leak_target_counts_post_requests() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let leak_url = upstream.token_url.replace("/token", "/leak");

    let response = reqwest::Client::new().post(leak_url).send().await?;

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(upstream.state.leak_hits.load(Ordering::SeqCst), 1);
    upstream.shutdown().await;
    Ok(())
}

/// The token endpoint answering with a 307 must not be followed: credentials
/// must never leave for a redirect target.
#[tokio::test(flavor = "multi_thread")]
async fn token_endpoint_redirect_is_refused_without_following() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn_mode(TokenMode::Redirect, 3600).await;
    // The token exchange runs during the eager upstream connect, so the
    // redirect refusal arrives as the downstream initialize's error.
    let message = crate::client_credentials::fixture::rejected_connect_message(
        &upstream,
        &[],
        "redirecting token endpoint",
    )
    .await?;
    assert_sanitized("token redirect error", &message);
    assert!(
        message.contains("redirect"),
        "expected the safe redirect refusal, got: {message}"
    );
    // The redirect target was never contacted: no credentials left the proxy.
    assert_eq!(upstream.state.leak_hits.load(Ordering::SeqCst), 0);

    upstream.shutdown().await;
    Ok(())
}

/// The token endpoint failing with an OAuth error yields the bounded
/// diagnostic — HTTP status plus at most the allowlisted standard error code —
/// never the provider's prose.
#[tokio::test(flavor = "multi_thread")]
async fn token_endpoint_error_is_status_and_allowlisted_code_only() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn_mode(
        TokenMode::Error {
            status: 400,
            code: "invalid_grant",
        },
        3600,
    )
    .await;
    let message = crate::client_credentials::fixture::rejected_connect_message(
        &upstream,
        &[],
        "failing token endpoint",
    )
    .await?;
    assert!(
        message.contains("HTTP 400") && message.contains("invalid_grant"),
        "expected the bounded status+code diagnostic, got: {message}"
    );
    assert_sanitized("token endpoint error", &message);
    assert!(
        !message.contains("provider prose"),
        "provider description leaked: {message}"
    );
    // Error mode issues no token; the recorded form is the exchange attempt.
    assert_eq!(
        upstream.state.token_forms.lock().unwrap().len(),
        1,
        "exactly one exchange attempt"
    );

    upstream.shutdown().await;
    Ok(())
}

/// A redirecting MCP endpoint is refused too: the bearer never leaves for a
/// redirect target.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_endpoint_redirect_is_refused_without_following() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let proxy = start(&upstream, &secret).await?;
    let peer = proxy.peer().clone();

    peer.call_tool(echo_args("warm up")).await?;
    let requests_before = upstream.state.mcp_requests.load(Ordering::SeqCst);

    // Every POST after this point redirects to the leak target.
    upstream
        .state
        .redirect_mcp_after
        .store(requests_before, std::sync::atomic::Ordering::SeqCst);

    let err = peer
        .call_tool(echo_args("redirect me"))
        .await
        .expect_err("redirecting MCP endpoint must fail the call");
    let message = err.to_string();
    assert_sanitized("MCP redirect error", &message);
    assert!(
        !message.contains(&upstream.mcp_url),
        "URL leaked: {message}"
    );
    assert_eq!(
        upstream.state.leak_hits.load(Ordering::SeqCst),
        0,
        "the redirect must never be followed"
    );

    proxy.cancel().await?;
    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

/// Sentinels that must never appear on a downstream surface or stderr for
/// the initialize/recovery failures below: distinct message and data
/// sentinels, the client secret, any issued token.
const INIT_SENTINELS: &[&str] = &[
    INIT_MSG_SENTINEL,
    INIT_DATA_SENTINEL,
    CLIENT_SECRET,
    TOKEN_PREFIX,
];

fn assert_no_init_sentinels(label: &str, text: &str) {
    for sentinel in INIT_SENTINELS {
        assert!(
            !text.contains(sentinel),
            "{label} leaked {sentinel}: {text}"
        );
    }
}

/// B1: rmcp builds session-recovery errors *after* the HTTP adapter already
/// ran. A session-bound POST gets 404 → `SessionExpired` → automatic
/// re-initialization → the answer is an HTTP 200 JSON-RPC error echoing the
/// presented token in message and data → `ReinitializationFailed`, whose
/// Display the old catch-all forwarded verbatim. The complete downstream
/// serialized frame must now carry only the static text, under hostile
/// trace-level logging, with stderr drained to EOF.
#[tokio::test(flavor = "multi_thread")]
async fn stateful_404_reinitialization_failure_is_replaced_with_safe_text() -> Result<()> {
    harnx_core::require_nextest();
    let upstream = ClientCredentialsUpstream::spawn_stateful().await;
    let (dir, secret) = write_client_secret_file();
    let args = oauth_args(&upstream, &secret, &[]);
    let (proxy, _methods, stderr) = spawn_proxy_client_env(&args, HOSTILE_LOGGING).await?;
    let peer = proxy.peer().clone();

    peer.call_tool(echo_args("warm up")).await?;
    assert!(
        upstream.state.initialize_count.load(Ordering::SeqCst) >= 1,
        "the warm-up connection must have initialized a session"
    );
    upstream.state.expire_sessions.store(true, Ordering::SeqCst);
    upstream.state.fail_initialize.store(true, Ordering::SeqCst);

    let err = peer
        .call_tool(echo_args("after session expiry"))
        .await
        .expect_err("session recovery failure must surface as an error");
    let rmcp::service::ServiceError::McpError(frame) = err else {
        panic!("expected the sanitized error frame, got: {err}");
    };
    let serialized = serde_json::to_string(&frame).expect("serialize downstream frame");
    assert!(
        serialized.contains(SAFE_UPSTREAM_FAILURE),
        "downstream frame must carry the static text: {serialized}"
    );
    assert_no_init_sentinels("downstream frame", &serialized);

    // The 404 really drove a re-initialization, and it failed — that is the
    // B1 recovery path, not a plain transport failure.
    assert_eq!(
        upstream.state.initialize_count.load(Ordering::SeqCst),
        2,
        "session expiry must trigger exactly one re-initialization"
    );

    proxy.cancel().await?;
    assert!(
        stderr_wait_eof(&stderr, std::time::Duration::from_secs(10)).await,
        "child stderr must reach EOF after shutdown"
    );
    let drained = stderr.text();
    assert!(
        drained.contains("starting in client-credentials mode"),
        "app positive control missing from drained stderr:\n{drained}"
    );
    assert_no_init_sentinels("drained stderr", &drained);

    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

/// B9: a decoded JSON-RPC error answering the *initial* authenticated
/// initialize (HTTP 200, accepted token) echoes the presented credential in
/// message and data. The connect path must reach its non-transport static
/// fallback — the fixture's counters prove the request got through auth and
/// was answered as a service error — and no echo prose may reach the
/// downstream error or the drained stderr.
#[tokio::test(flavor = "multi_thread")]
async fn initial_initialize_jsonrpc_error_is_replaced_with_safe_text() -> Result<()> {
    harnx_core::require_nextest();
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    upstream.state.fail_initialize.store(true, Ordering::SeqCst);
    let args = oauth_args(&upstream, &secret, &[]);
    let (message, stderr) = spawn_proxy_client_rejected(&args, HOSTILE_LOGGING).await?;
    for surface in [("downstream error", &message), ("drained stderr", &stderr)] {
        let (label, text) = surface;
        assert!(
            text.contains(SAFE_UPSTREAM_FAILURE),
            "{label} must carry the static fallback: {text}"
        );
        assert_no_init_sentinels(label, text);
    }
    // The initialize reached the fixture and its auth guard accepted the
    // token, so the failure is the decoded JSON-RPC error — the B9
    // non-transport fallback arm — not a transport failure.
    assert_eq!(
        upstream.state.initialize_count.load(Ordering::SeqCst),
        1,
        "the authenticated initialize must have reached the upstream handler"
    );
    assert_eq!(
        upstream.state.unauthorized_401s.load(Ordering::SeqCst),
        0,
        "the initialize must have passed auth before failing as a service error"
    );

    upstream.shutdown().await;
    drop(dir);
    Ok(())
}
