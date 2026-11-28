//! Client-credentials mode is opt-in and validated before any network use:
//! every rejection here must fire without contacting either endpoint —
//! proved against a live fixture's request counters and redirect leak target,
//! not an idle port — and no connect error or drained stderr line may
//! contain the raw configured URLs, the client secret, or any other
//! credential.

use anyhow::Result;
use std::sync::atomic::Ordering;

use crate::client_credentials::fixture::{
    wait_until_settled, write_client_secret_file, ClientCredentialsUpstream, CLIENT_ID,
    CLIENT_SECRET,
};
use crate::common::spawn_proxy_client_rejected;

fn args(base: &[&str]) -> Vec<String> {
    base.iter().map(|arg| arg.to_string()).collect()
}

/// Spawns the proxy expecting its eager upstream connect to be rejected, and
/// returns the connect error alongside the EOF-drained stderr. The drained
/// capture is the authoritative surface for "nothing unsafe was logged":
/// the error string only embeds a stderr snapshot at failure time.
async fn rejected(base: &[&str], env: &[(&str, &str)]) -> Result<(String, String)> {
    harnx_core::require_nextest();
    spawn_proxy_client_rejected(&args(base), env).await
}

/// Asserts a rejected invocation end to end:
///
/// - every expected static rejection text appears in the connect error;
/// - no raw configured URL, the client secret, or any sentinel listed in
///   `hidden` appears on the connect error or the drained stderr;
/// - after the fixture counters settle (quiet period), the token endpoint,
///   the MCP endpoint and the redirect leak target all saw zero requests.
///
/// `hidden` covers credentials that are not the fixture's own: bearer flag
/// values, embedded userinfo, missing-file paths.
async fn assert_rejection_safety(
    upstream: &ClientCredentialsUpstream,
    message: &str,
    stderr: &str,
    expected: &[&str],
    hidden: &[&str],
) -> Result<()> {
    for text in expected {
        assert!(
            message.contains(text),
            "expected rejection containing {text:?}, got: {message}"
        );
    }
    let mut needles: Vec<String> = vec![
        upstream.mcp_url.clone(),
        upstream.token_url.clone(),
        CLIENT_SECRET.to_string(),
    ];
    needles.extend(hidden.iter().map(|needle| needle.to_string()));
    for (label, text) in [("error", message), ("stderr", stderr)] {
        for needle in &needles {
            assert!(
                !text.contains(needle.as_str()),
                "{label} leaked {needle:?}: {text}"
            );
        }
    }
    // Quiet-period settle before reading the counters: a request still in
    // flight when the rejection returned must land before zero is trusted.
    wait_until_settled(|| {
        upstream.state.token_exchanges.load(Ordering::SeqCst)
            + upstream.state.mcp_requests.load(Ordering::SeqCst)
            + upstream.state.leak_hits.load(Ordering::SeqCst)
    })
    .await?;
    assert_eq!(
        upstream.state.token_exchanges.load(Ordering::SeqCst),
        0,
        "no client-secret grant may be attempted"
    );
    assert_eq!(
        upstream.state.mcp_requests.load(Ordering::SeqCst),
        0,
        "the MCP endpoint must not be contacted"
    );
    assert_eq!(
        upstream.state.leak_hits.load(Ordering::SeqCst),
        0,
        "the redirect leak target must not be reached"
    );
    Ok(())
}

/// Runs one single-case rejection end to end: collects the rejected connect,
/// asserts the full safety contract (static text, no leak on either surface,
/// zero contact after the quiet period), then shuts the fixture down.
/// `base` is owned because it quotes the fixture URLs: building it ends the
/// borrow before `upstream` moves in. Multi-case tests keep their own loops
/// and reuse one upstream, so they call [`rejected`] and
/// [`assert_rejection_safety`] directly.
async fn rejected_case(
    upstream: ClientCredentialsUpstream,
    base: Vec<String>,
    env: &[(&str, &str)],
    expected: &[&str],
    hidden: &[&str],
) -> Result<()> {
    harnx_core::require_nextest();
    let (message, stderr) = spawn_proxy_client_rejected(&base, env).await?;
    assert_rejection_safety(&upstream, &message, &stderr, expected, hidden).await?;
    upstream.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_bearer_token_flag_conflict() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let secret = secret.display().to_string();
    let base = args(&[
        "--url",
        &upstream.mcp_url,
        "--oauth-token-url",
        &upstream.token_url,
        "--oauth-client-id",
        CLIENT_ID,
        "--oauth-client-secret-file",
        &secret,
        "--bearer-token",
        "other-token",
    ]);
    rejected_case(
        upstream,
        base,
        &[],
        &["--oauth-token-url cannot be combined with --bearer-token"],
        &["other-token"],
    )
    .await?;
    drop(dir);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_bearer_token_env_conflict() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let secret = secret.display().to_string();
    let base = args(&[
        "--url",
        &upstream.mcp_url,
        "--oauth-token-url",
        &upstream.token_url,
        "--oauth-client-id",
        CLIENT_ID,
        "--oauth-client-secret-file",
        &secret,
    ]);
    rejected_case(
        upstream,
        base,
        &[("MCP_REMOTE_BEARER_TOKEN", "env-token")],
        &["--oauth-token-url cannot be combined with --bearer-token"],
        &["env-token"],
    )
    .await?;
    drop(dir);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_custom_authorization_header_in_any_case() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let secret = secret.display().to_string();
    for header in ["Authorization: x", "aUtHoRiZaTiOn: x", " authorization : x"] {
        let base = &[
            "--url",
            &upstream.mcp_url,
            "--oauth-token-url",
            &upstream.token_url,
            "--oauth-client-id",
            CLIENT_ID,
            "--oauth-client-secret-file",
            &secret,
            "--header",
            header,
        ];
        let (message, stderr) = rejected(base, &[]).await?;
        assert_rejection_safety(
            &upstream,
            &message,
            &stderr,
            &["cannot be combined with a custom Authorization --header"],
            &[header],
        )
        .await?;
    }
    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_requires_client_id_and_secret_file() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let secret = secret.display().to_string();
    let missing_id = &[
        "--url",
        &upstream.mcp_url,
        "--oauth-token-url",
        &upstream.token_url,
        "--oauth-client-secret-file",
        &secret,
    ];
    let (message, stderr) = rejected(missing_id, &[]).await?;
    assert_rejection_safety(
        &upstream,
        &message,
        &stderr,
        &["--oauth-token-url requires --oauth-client-id"],
        &[],
    )
    .await?;

    let missing_secret = &[
        "--url",
        &upstream.mcp_url,
        "--oauth-token-url",
        &upstream.token_url,
        "--oauth-client-id",
        CLIENT_ID,
    ];
    let (message, stderr) = rejected(missing_secret, &[]).await?;
    assert_rejection_safety(
        &upstream,
        &message,
        &stderr,
        &["--oauth-token-url requires --oauth-client-secret-file"],
        &[],
    )
    .await?;
    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_unreadable_secret_file_before_network() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let base = args(&[
        "--url",
        &upstream.mcp_url,
        "--oauth-token-url",
        &upstream.token_url,
        "--oauth-client-id",
        CLIENT_ID,
        "--oauth-client-secret-file",
        "/nonexistent/harnx/no-such-secret",
    ]);
    rejected_case(
        upstream,
        base,
        &[],
        &["cannot read --oauth-client-secret-file"],
        &["/nonexistent/harnx/no-such-secret"],
    )
    .await?;
    Ok(())
}

/// Both endpoints are checked independently: the token endpoint may be fine
/// while the MCP URL is plaintext non-loopback, and vice versa. The offending
/// side must be a real non-loopback host (`example.invalid`), the safe side a
/// live fixture whose counters prove zero contact.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_non_https_non_loopback_endpoints_independently() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (dir, secret) = write_client_secret_file();
    let secret = secret.display().to_string();
    // Token endpoint non-HTTPS, MCP endpoint a live loopback fixture.
    let token_plain = &[
        "--url",
        &upstream.mcp_url,
        "--oauth-token-url",
        "http://example.invalid/token",
        "--oauth-client-id",
        CLIENT_ID,
        "--oauth-client-secret-file",
        &secret,
    ];
    let (message, stderr) = rejected(token_plain, &[]).await?;
    assert_rejection_safety(
        &upstream,
        &message,
        &stderr,
        &[
            "non-HTTPS token endpoint",
            "use HTTPS, loopback, or --insecure",
        ],
        &["example.invalid"],
    )
    .await?;

    // MCP endpoint non-HTTPS, token endpoint a live loopback fixture.
    let mcp_plain = &[
        "--url",
        "http://example.invalid/mcp",
        "--oauth-token-url",
        &upstream.token_url,
        "--oauth-client-id",
        CLIENT_ID,
        "--oauth-client-secret-file",
        &secret,
    ];
    let (message, stderr) = rejected(mcp_plain, &[]).await?;
    assert_rejection_safety(
        &upstream,
        &message,
        &stderr,
        &[
            "non-HTTPS remote MCP endpoint",
            "use HTTPS, loopback, or --insecure",
        ],
        &["example.invalid"],
    )
    .await?;
    upstream.shutdown().await;
    drop(dir);
    Ok(())
}

/// URLs with embedded userinfo (username or password) are rejected before
/// either endpoint is contacted, and neither the constructed URL nor the
/// credential fragment may reach the error or the drained stderr.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_embedded_url_credentials() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    // (mcp URL, token URL, extra hidden needles beyond the URLs themselves)
    let token_with_password =
        upstream
            .token_url
            .replacen("http://", "http://user:token-secret@", 1);
    let mcp_with_user = upstream
        .mcp_url
        .replacen("http://", "http://embedded-user@", 1);
    let cases: &[(String, String, Vec<String>)] = &[
        (
            upstream.mcp_url.clone(),
            token_with_password.clone(),
            vec![token_with_password.clone(), "token-secret".into()],
        ),
        (
            mcp_with_user.clone(),
            upstream.token_url.clone(),
            vec![mcp_with_user.clone(), "embedded-user".into()],
        ),
    ];
    for (mcp_url, token_url, hidden) in cases {
        let (dir, secret) = write_client_secret_file();
        let secret = secret.display().to_string();
        let base = &[
            "--url",
            mcp_url,
            "--oauth-token-url",
            token_url,
            "--oauth-client-id",
            CLIENT_ID,
            "--oauth-client-secret-file",
            &secret,
        ];
        let (message, stderr) = rejected(base, &[]).await?;
        assert_rejection_safety(
            &upstream,
            &message,
            &stderr,
            &["must not contain embedded user credentials"],
            &hidden.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .await?;
        drop(dir);
    }
    upstream.shutdown().await;
    Ok(())
}

/// Native mode (per-user credentials) must reject the process-wide
/// client-credentials flag before any NATS connection. The mixed-mode child
/// exits before serving stdio, so the static rejection is asserted on the
/// EOF-drained stderr, the authoritative surface — the connect error's
/// snapshot may still be empty.
#[tokio::test(flavor = "multi_thread")]
async fn native_mode_rejects_oauth_token_url() -> Result<()> {
    let upstream = ClientCredentialsUpstream::spawn().await;
    let base = &[
        "--url",
        &upstream.mcp_url,
        "--credentials-key-file",
        "/nonexistent/key",
        "--oauth-token-url",
        &upstream.token_url,
        "--oauth-client-id",
        CLIENT_ID,
        "--oauth-client-secret-file",
        "/nonexistent/secret",
    ];
    let (message, stderr) = rejected(base, &[]).await?;
    assert!(
        stderr.contains("native mode does not accept --oauth-token-url"),
        "expected the native-mode rejection on drained stderr, got: {stderr}"
    );
    assert_rejection_safety(
        &upstream,
        &message,
        &stderr,
        &[],
        &["/nonexistent/key", "/nonexistent/secret"],
    )
    .await?;
    upstream.shutdown().await;
    Ok(())
}

// ---- B4: non-HTTP schemes must be rejected before any endpoint contact ----

/// Client-credentials argv with explicit endpoints, for the endpoint-policy
/// tests that vary one URL while the other side stays a real fixture whose
/// request counters can prove zero contact.
fn endpoint_args(mcp_url: &str, token_url: &str, secret: &str, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "--url",
        mcp_url,
        "--oauth-token-url",
        token_url,
        "--oauth-client-id",
        CLIENT_ID,
        "--oauth-client-secret-file",
        secret,
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    args.extend(extra.iter().map(|arg| arg.to_string()));
    args
}

/// A parseable non-HTTP MCP URL on a loopback host (`file://localhost/mcp`,
/// `ftp://localhost/mcp`) used to pass the loopback gate, so the provider
/// sent the client-secret grant before rmcp rejected the MCP URL. The
/// scheme+host preflight must fire first, with or without `--insecure`, and
/// the token endpoint (here a real fixture) must see zero grants.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_non_http_mcp_url_before_any_grant() -> Result<()> {
    harnx_core::require_nextest();
    let cases: &[(&str, &[&str])] = &[
        ("file://localhost/mcp", &[]),
        ("file://localhost/mcp", &["--insecure"]),
        ("ftp://localhost/mcp", &["--insecure"]),
    ];
    for (mcp_url, extra) in cases {
        let upstream = ClientCredentialsUpstream::spawn().await;
        let (dir, secret) = write_client_secret_file();
        let args = endpoint_args(
            mcp_url,
            &upstream.token_url,
            &secret.display().to_string(),
            extra,
        );
        let (message, stderr) = spawn_proxy_client_rejected(&args, &[]).await?;
        assert_rejection_safety(
            &upstream,
            &message,
            &stderr,
            &["remote MCP URL must be an absolute http(s) URL"],
            &[mcp_url],
        )
        .await?;
        upstream.shutdown().await;
        drop(dir);
    }
    Ok(())
}

/// The token endpoint URL gets the same preflight: a non-HTTP scheme must be
/// rejected before any MCP request, with and without `--insecure`.
#[tokio::test(flavor = "multi_thread")]
async fn oauth_mode_rejects_non_http_token_url_before_any_contact() -> Result<()> {
    harnx_core::require_nextest();
    let cases: &[&[&str]] = &[&[], &["--insecure"]];
    for extra in cases {
        let upstream = ClientCredentialsUpstream::spawn().await;
        let (dir, secret) = write_client_secret_file();
        let args = endpoint_args(
            &upstream.mcp_url,
            "ftp://localhost/token",
            &secret.display().to_string(),
            extra,
        );
        let (message, stderr) = spawn_proxy_client_rejected(&args, &[]).await?;
        assert_rejection_safety(
            &upstream,
            &message,
            &stderr,
            &["token URL must be an absolute http(s) URL"],
            &["ftp://localhost/token"],
        )
        .await?;
        upstream.shutdown().await;
        drop(dir);
    }
    Ok(())
}
