//! TLS identity and CA sharing: an mTLS upstream that requires a client
//! certificate is only reachable when both the token client and the MCP
//! client use the same configured TLS identity and CA.

use anyhow::Result;
use std::sync::atomic::Ordering;

use crate::client_credentials::fixture::{
    echo_args, oauth_args, write_client_secret_file, ClientCredentialsUpstream, TlsClientPaths,
    TokenMode, CLIENT_SECRET, TOKEN_PREFIX,
};
use crate::common::{spawn_proxy_client_env, spawn_proxy_client_rejected, text_content};

fn tls_identity_flags(tls: &TlsClientPaths) -> [String; 4] {
    [
        "--tls-cert".to_string(),
        tls.cert.display().to_string(),
        "--tls-key".to_string(),
        tls.key.display().to_string(),
    ]
}

async fn oauth_peer(
    upstream: &ClientCredentialsUpstream,
    secret_file: &std::path::Path,
    extra: &[String],
) -> Result<rmcp::service::RunningService<rmcp::RoleClient, crate::common::TestClientHandler>> {
    let mut args = oauth_args(upstream, secret_file, &[]);
    args.extend(extra.iter().cloned());
    match spawn_proxy_client_env(&args, &[]).await {
        Ok((proxy, _, _)) => Ok(proxy),
        Err(err) => {
            // Distinguish address-family problems from TLS alerts, which the
            // fixture listener reports separately. This never enters child stderr.
            let addresses = tokio::net::lookup_host(("localhost", 0))
                .await
                .map(|addrs| addrs.map(|addr| addr.ip()).collect::<Vec<_>>());
            Err(err.context(format!(
                "fixture binds IPv4 loopback; localhost resolves to {addresses:?}"
            )))
        }
    }
}

/// The server requires a client certificate on every connection, so the
/// token exchange and the MCP call both succeed only when the shared
/// TLS identity and CA are applied to both clients.
#[tokio::test(flavor = "multi_thread")]
async fn shared_tls_identity_and_ca_reach_both_clients() -> Result<()> {
    harnx_core::require_nextest();
    let (upstream, tls) = ClientCredentialsUpstream::spawn_tls(TokenMode::Issue, 3600, false).await;
    let (dir, secret) = write_client_secret_file();
    let mut flags = vec!["--tls-ca".to_string(), tls.ca.display().to_string()];
    flags.extend(tls_identity_flags(&tls));
    let proxy = oauth_peer(&upstream, &secret, &flags).await?;
    let peer = proxy.peer().clone();

    let result = peer.call_tool(echo_args("over mtls")).await?;
    let value: serde_json::Value = serde_json::from_str(&text_content(&result)).expect("echo JSON");
    assert_eq!(value["text"], "over mtls");

    // Both endpoints were reached: the token exchange succeeded over TLS
    // with client identity, and the upstream saw exactly one initialize.
    assert_eq!(upstream.state.token_exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(upstream.state.initialize_count.load(Ordering::SeqCst), 1);

    proxy.cancel().await?;
    upstream.shutdown().await;
    drop(dir);
    drop(tls);
    Ok(())
}

/// Omit only the custom CA. The client identity is still configured, but the
/// token exchange fails before reaching HTTP and MCP does not connect.
#[tokio::test(flavor = "multi_thread")]
async fn missing_custom_ca_fails_token_exchange_safely() -> Result<()> {
    harnx_core::require_nextest();
    let (upstream, tls) = ClientCredentialsUpstream::spawn_tls(TokenMode::Issue, 3600, false).await;
    let (_dir, secret) = write_client_secret_file();
    let mut args = oauth_args(&upstream, &secret, &[]);
    args.extend(tls_identity_flags(&tls));
    let (message, stderr) = spawn_proxy_client_rejected(&args, &[]).await?;

    assert!(
        message.contains("token endpoint request failed"),
        "expected a safe token request failure: {message}"
    );
    for text in [&message, &stderr] {
        assert!(!text.contains(CLIENT_SECRET), "secret leaked: {text}");
        assert!(!text.contains(TOKEN_PREFIX), "token leaked: {text}");
        assert!(
            !text.contains(&upstream.token_url) && !text.contains(&upstream.mcp_url),
            "URL leaked: {text}"
        );
    }
    assert!(upstream.state.token_forms.lock().unwrap().is_empty());
    assert_eq!(upstream.state.mcp_requests.load(Ordering::SeqCst), 0);

    upstream.shutdown().await;
    drop(tls);
    Ok(())
}
