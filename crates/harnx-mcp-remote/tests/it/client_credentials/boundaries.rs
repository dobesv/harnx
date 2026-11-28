//! Mode boundaries: the plain proxy keeps its defaults and log text, and
//! client-credentials mode owns the stderr boundary — app-owned safe targets
//! only, even under hostile trace-level logging, and never the configured
//! URLs, tokens or malformed `--header` arguments.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::service::RunningService;
use rmcp::RoleClient;

use crate::client_credentials::fixture::{
    call_tool_args, echo_args, oauth_args, write_client_secret_file, ClientCredentialsUpstream,
    CLIENT_SECRET, TOKEN_PREFIX,
};
use crate::common::{
    spawn_http_test_server, spawn_proxy_client_env, spawn_proxy_client_rejected, stderr_wait_eof,
    text_content, CapturedStderr, TestClientHandler,
};

use super::fixture::SAFE_UPSTREAM_FAILURE;

/// `HARNX_LOG_FILTER` takes one literal prefix. `reqwest` enables real
/// dependency `log` records in plain mode; OAuth mode must override it.
/// JSON lines let us check the target separately from the message.
pub(super) const HOSTILE_LOGGING: &[(&str, &str)] = &[
    ("HARNX_LOG_LEVEL", "trace"),
    ("HARNX_LOG_FILTER", "reqwest"),
    ("HARNX_LOG_FORMAT", "json"),
];

type RunningProxy = RunningService<RoleClient, TestClientHandler>;

async fn cancel_and_drain(proxy: RunningProxy, stderr: &CapturedStderr) -> Result<String> {
    proxy.cancel().await?;
    anyhow::ensure!(
        stderr_wait_eof(stderr, Duration::from_secs(10)).await,
        "child stderr must reach EOF after shutdown"
    );
    Ok(stderr.text())
}

async fn start_oauth_proxy(
    upstream: &ClientCredentialsUpstream,
    secret: &std::path::Path,
) -> Result<(RunningProxy, CapturedStderr)> {
    let args = oauth_args(upstream, secret, &[]);
    let (proxy, _methods, stderr) = spawn_proxy_client_env(&args, HOSTILE_LOGGING).await?;
    Ok((proxy, stderr))
}

#[derive(serde::Deserialize)]
struct StderrRecord {
    target: String,
    message: String,
}

fn stderr_records(text: &str) -> Result<Vec<StderrRecord>> {
    text.lines()
        .map(|line| {
            serde_json::from_str(line).with_context(|| format!("invalid log record: {line}"))
        })
        .collect()
}

fn assert_oauth_stderr(text: &str, upstream: &ClientCredentialsUpstream) -> Result<()> {
    let records = stderr_records(text)?;
    for record in &records {
        assert!(
            record.target.starts_with("harnx"),
            "non-app target in OAuth stderr: {}\n{text}",
            record.target
        );
    }
    assert!(
        records.iter().any(|record| record
            .message
            .contains("starting in client-credentials mode")),
        "app startup record missing from drained stderr:\n{text}"
    );
    assert_no_credentials("drained stderr", text, upstream);
    Ok(())
}

/// Sentinels that must never reach a downstream error or stderr: the client
/// secret, any issued token and both configured URLs.
fn assert_no_credentials(label: &str, text: &str, upstream: &ClientCredentialsUpstream) {
    assert!(!text.contains(CLIENT_SECRET), "{label} secret: {text}");
    assert!(!text.contains(TOKEN_PREFIX), "{label} token: {text}");
    assert!(
        !text.contains(&upstream.mcp_url) && !text.contains(&upstream.token_url),
        "{label} configured URL: {text}"
    );
}

/// Plain proxy mode is untouched: it still logs the upstream URL, and tool
/// calls succeed with the same stdio lifecycle.
#[tokio::test(flavor = "multi_thread")]
async fn plain_proxy_defaults_are_unchanged() -> Result<()> {
    harnx_core::require_nextest();
    let server = spawn_http_test_server(None, false).await?;
    let url = format!("http://127.0.0.1:{}/mcp", server.port);
    let args: Vec<String> = ["--url", &url].iter().map(|a| a.to_string()).collect();
    let (proxy, _methods, stderr) = spawn_proxy_client_env(&args, &[]).await?;
    let peer = proxy.peer().clone();

    let result = peer.call_tool(echo_args("plain default")).await?;
    let value: serde_json::Value = serde_json::from_str(&text_content(&result)).expect("echo JSON");
    assert_eq!(value["text"], "plain default");

    let logged = cancel_and_drain(proxy, &stderr).await?;
    assert!(
        logged.contains("starting, proxying to") && logged.contains(&url),
        "plain mode must keep its unchanged startup log with the URL: {logged}"
    );

    server.shutdown().await;
    Ok(())
}

/// A successful call keeps its output intact without logging that output, the
/// configured URLs or credentials, even with a dependency prefix requested.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_stderr_keeps_app_targets_and_excludes_credentials() -> Result<()> {
    harnx_core::require_nextest();
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (_dir, secret) = write_client_secret_file();
    let (proxy, stderr) = start_oauth_proxy(&upstream, &secret).await?;

    const MARKER: &str = "stderr-leak-marker-1459";
    let result = proxy.peer().call_tool(echo_args(MARKER)).await?;
    let drained = cancel_and_drain(proxy, &stderr).await?;
    assert_ne!(result.is_error, Some(true), "call must succeed");
    assert_eq!(
        text_content(&result),
        serde_json::json!({ "text": MARKER }).to_string(),
        "successful tool output must remain intact"
    );
    assert_oauth_stderr(&drained, &upstream)?;
    assert!(
        !drained.contains(MARKER),
        "proxied payload leaked into stderr:\n{drained}"
    );

    upstream.shutdown().await;
    Ok(())
}

async fn failed_tool_frame(proxy: &RunningProxy, tool: &'static str) -> Result<String> {
    let err = proxy
        .peer()
        .call_tool(call_tool_args(tool))
        .await
        .expect_err("the fixture tool must fail");
    let rmcp::service::ServiceError::McpError(frame) = err else {
        anyhow::bail!("{tool} must return a JSON-RPC error frame: {err}");
    };
    Ok(serde_json::to_string(&frame)?)
}

async fn credential_echo_failures(proxy: &RunningProxy) -> Result<[String; 2]> {
    Ok([
        failed_tool_frame(proxy, "fail_jsonrpc").await?,
        failed_tool_frame(proxy, "fail_with_retired_token").await?,
    ])
}

/// Use the same live fixture and repeat the failed calls with ordinary fixed
/// bearer auth. This runs the production plain-proxy stderr path under the
/// same user settings, with no switch to weaken the OAuth path.
async fn plain_proxy_control(
    upstream: &ClientCredentialsUpstream,
    current: &str,
    retired: &str,
) -> Result<()> {
    let args = ["--url", &upstream.mcp_url, "--bearer-token", current].map(str::to_owned);
    let (proxy, _methods, stderr) = spawn_proxy_client_env(&args, HOSTILE_LOGGING).await?;
    let [current_error, retired_error] = credential_echo_failures(&proxy).await?;
    let drained = cancel_and_drain(proxy, &stderr).await?;

    assert!(current_error.contains(&format!("upstream rejected: saw bearer Bearer {current}")));
    assert!(retired_error.contains(&format!("stale bearer {retired} was rejected here")));
    assert!(
        stderr_records(&drained)?
            .iter()
            .any(|record| record.target == "reqwest::connect"),
        "plain control did not emit a real reqwest connection record:\n{drained}"
    );
    Ok(())
}

/// B8: failures echoing current and retired credentials must stay safe in
/// both the downstream error and all stderr records, even after cancellation.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_failure_stderr_is_drained_and_safe_under_trace_logging() -> Result<()> {
    harnx_core::require_nextest();
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (_dir, secret) = write_client_secret_file();
    let (proxy, stderr) = start_oauth_proxy(&upstream, &secret).await?;

    // A deliberate 401 rotates the token. No timer or polling is needed:
    // the successful replay has already presented the replacement.
    let retired = format!("{TOKEN_PREFIX}1");
    let current = format!("{TOKEN_PREFIX}2");
    upstream.state.reject_token(&retired);
    let replay = proxy.peer().call_tool(echo_args("rotate token")).await?;
    assert_ne!(replay.is_error, Some(true));
    assert_eq!(upstream.state.token_exchanges.load(Ordering::SeqCst), 2);

    let [current_error, retired_error] = credential_echo_failures(&proxy).await?;
    let drained = cancel_and_drain(proxy, &stderr).await?;
    assert_oauth_stderr(&drained, &upstream)?;
    for error in [&current_error, &retired_error] {
        assert!(
            error.contains(SAFE_UPSTREAM_FAILURE),
            "unsafe error: {error}"
        );
        assert_no_credentials("downstream error", error, &upstream);
    }
    for surface in [&drained, &current_error, &retired_error] {
        assert!(
            !surface.contains("upstream rejected") && !surface.contains("stale bearer"),
            "upstream error prose leaked:\n{surface}"
        );
    }

    plain_proxy_control(&upstream, &current, &retired).await?;
    upstream.shutdown().await;
    Ok(())
}

/// B3: mixed native+OAuth mode is rejected before the native startup log,
/// which prints the raw configured URL. The rejection surfaces on the
/// child's stderr (the process exits before serving stdio, so the
/// downstream connect error may be a bare "connection closed"); the fully
/// drained stderr must carry the static message and no URL sentinel.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_native_oauth_mode_is_rejected_before_the_raw_url_log() -> Result<()> {
    harnx_core::require_nextest();
    const URL_SENTINEL: &str = "native-url-sentinel-3028";
    const TOKEN_URL_SENTINEL: &str = "native-token-url-sentinel-3028";
    let args: Vec<String> = [
        "--url",
        &format!("http://{URL_SENTINEL}@127.0.0.1:9/mcp"),
        "--credentials-key-file",
        "/nonexistent/key",
        "--oauth-token-url",
        &format!("http://{TOKEN_URL_SENTINEL}@127.0.0.1:9/token"),
        "--oauth-client-id",
        "svc-account",
        "--oauth-client-secret-file",
        "/nonexistent/secret",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    let (message, stderr) = spawn_proxy_client_rejected(&args, &[]).await?;
    for (label, text) in [("error", &message), ("stderr", &stderr)] {
        assert!(!text.contains(URL_SENTINEL), "{label} leaked URL: {text}");
        assert!(
            !text.contains(TOKEN_URL_SENTINEL),
            "{label} leaked token URL: {text}"
        );
        assert!(
            !text.contains("127.0.0.1:9"),
            "{label} leaked the raw configured URL: {text}"
        );
        assert!(
            !text.contains("starting native mode"),
            "{label} shows the raw-URL native startup log: {text}"
        );
    }
    assert!(
        stderr.contains("native mode does not accept --oauth-token-url"),
        "stderr must carry the static rejection: {stderr}"
    );
    Ok(())
}

/// B3: a malformed no-colon `Authorization ...` `--header` argument bypassed
/// the Authorization-name conflict check, and the shared parser then echoed
/// the raw argument downstream and to stderr. OAuth mode now rejects it with
/// static text before any client exists, so neither endpoint is contacted.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_malformed_header_without_leaking_it() -> Result<()> {
    harnx_core::require_nextest();
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    const HEADER_SENTINEL: &str = "malformed-header-sentinel-771";
    let args = oauth_args(
        &upstream,
        &secret,
        &[&format!("--header=Authorization Bearer {HEADER_SENTINEL}")],
    );
    let (message, stderr) = spawn_proxy_client_rejected(&args, &[]).await?;
    for surface in [("error", &message), ("stderr", &stderr)] {
        let (label, text) = surface;
        assert!(
            text.contains("invalid --header entry in client-credentials mode"),
            "{label} must carry the static rejection: {text}"
        );
        assert!(
            !text.contains(HEADER_SENTINEL),
            "{label} leaked the raw header argument: {text}"
        );
        assert_no_credentials(label, text, &upstream);
    }
    // Zero endpoint contact: the header gate runs before any HTTP client.
    assert_eq!(upstream.state.token_exchanges.load(Ordering::SeqCst), 0);
    assert_eq!(upstream.state.mcp_requests.load(Ordering::SeqCst), 0);

    upstream.shutdown().await;
    drop(dir);
    Ok(())
}
