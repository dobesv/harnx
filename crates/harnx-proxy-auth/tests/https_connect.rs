//! Integration test for header injection through HTTPS CONNECT tunnels.
//!
//! Uses the proxy library directly (not the binary) so we can control the
//! outbound TLS connector and test the full MITM flow end-to-end, including
//! the bug fixed in proxy.rs where `uri.host()` returned "" for tunnelled
//! HTTPS requests and jq host-match filters never fired.

use std::sync::Arc;

use anyhow::{Context, Result};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout, Duration};
use tokio_rustls::rustls::{self, pki_types};
use tokio_rustls::TlsAcceptor;

use harnx_proxy_auth::{ca, filter, proxy};
fn jaq_vars(
    sentinels: &Arc<harnx_proxy_auth::sentinel::Sentinels>,
) -> Arc<harnx_proxy_auth::filter::JaqVars> {
    Arc::new(
        harnx_proxy_auth::filter::JaqVars::new(sentinels, String::new(), Vec::new())
            .expect("jaq vars"),
    )
}

/// Starts an HTTPS test server using the provided cert+key (DER-encoded).
async fn spawn_https_server(
    cert_der_bytes: Vec<u8>,
    key_der_bytes: Vec<u8>,
) -> Result<(u16, oneshot::Sender<()>)> {
    let cert_der = pki_types::CertificateDer::from(cert_der_bytes);
    let key_der = pki_types::PrivateKeyDer::try_from(key_der_bytes)
        .map_err(|e| anyhow::anyhow!("wrap key: {e}"))?;

    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("build TLS config")?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accept_result = listener.accept() => {
                    let Ok((stream, _)) = accept_result else { break; };
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(stream).await else { return; };
                        let svc = service_fn(echo_headers);
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(tls), svc)
                            .await;
                    });
                }
            }
        }
    });

    Ok((port, shutdown_tx))
}

async fn echo_headers(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let headers: BTreeMap<String, String> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = serde_json::to_vec(&headers).unwrap_or_default();
    Ok(Response::builder()
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

/// Regression test for the tunnelled-HTTPS host bug.
///
/// Before the fix: `uri.host()` returned "" for inner HTTPS requests after
/// the CONNECT tunnel, so `.host == "localhost"` never matched — no headers
/// were injected.
///
/// After the fix: the proxy falls back to the `Host` header when `uri.host()`
/// is empty, so `.host == "localhost"` matches correctly.
#[tokio::test]
async fn header_injection_works_through_https_connect_tunnel() {
    // Workspace builds pull in both `aws-lc-rs` and `ring` rustls providers
    // transitively, so rustls' process-level auto-detect panics. Pin one here.
    // `.ok()` tolerates a provider already installed by an earlier test.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    timeout(Duration::from_secs(20), async {
        // Set up the proxy CA — keep the TempDir alive for the duration.
        let (ca_setup, _ca_temp_dir) = ca::setup().expect("proxy CA setup");
        let ca_cert_pem =
            std::fs::read_to_string(&ca_setup.cert_pem_path).expect("read CA cert PEM");

        // Generate a self-signed server cert for "localhost".
        let server_cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("gen server cert");
        let server_cert_der = server_cert.cert.der().to_vec();
        let server_key_der = server_cert.signing_key.serialize_der();

        // Spawn an HTTPS server using the self-signed cert.
        let (server_port, _server_shutdown) = spawn_https_server(server_cert_der, server_key_der)
            .await
            .expect("spawn HTTPS server");

        // Compile the jq filter that injects a test header for localhost.
        let filter_expr =
            r#"if .host == "localhost" then .headers["x-test-header"] = "hello-world" end"#;
        let compiled = filter::compile(filter_expr).expect("compile filter");
        let sentinels = Arc::new(harnx_proxy_auth::sentinel::Sentinels::generate());

        // Start the proxy with danger_accept_invalid_certs so it can connect
        // upstream to our self-signed test server.
        let proxy_port = proxy::start_proxy_danger_accept_invalid_certs(
            compiled,
            ca_setup,
            jaq_vars(&sentinels),
        )
        .await
        .expect("start proxy");

        sleep(Duration::from_millis(100)).await;

        // Build a reqwest client that routes HTTPS through the proxy (CONNECT
        // tunnel). `danger_accept_invalid_certs` is required because the
        // MITM leaf certs hudsucker generates have no Extended Key Usage,
        // which the macOS Security framework (used by rustls-platform-verifier
        // on Darwin) rejects with EkuError. The proxy CA is still pinned so
        // any verifier that *does* honour it sees a trusted chain.
        let proxy_ca = reqwest::tls::Certificate::from_pem(ca_cert_pem.as_bytes())
            .expect("parse proxy CA cert");

        let client = reqwest::Client::builder()
            .proxy(
                reqwest::Proxy::https(format!("http://127.0.0.1:{proxy_port}"))
                    .expect("build proxy"),
            )
            .add_root_certificate(proxy_ca)
            .danger_accept_invalid_certs(true)
            .build()
            .expect("build reqwest client");

        let response = client
            .get(format!("https://localhost:{server_port}/"))
            .send()
            .await
            .expect("send request");

        assert!(
            response.status().is_success(),
            "expected 200, got {}",
            response.status()
        );

        let body: Value = response.json().await.expect("parse JSON body");

        assert_eq!(
            body.get("x-test-header").and_then(Value::as_str),
            Some("hello-world"),
            "proxy did not inject x-test-header for tunnelled HTTPS — \
             host was not resolved from Host header. Full echoed headers: {body}"
        );
    })
    .await
    .expect("test timed out");
}

/// Test that hook filters can use sentinel jaq variables directly.
///
/// This covers hook-side sentinel interpolation without exporting sentinels into
/// process env. The filter matches a sentinel-backed Authorization header and
/// replaces it before forwarding request upstream.
#[tokio::test]
async fn hook_filter_can_use_sentinel_variables() {
    use harnx_proxy_auth::sentinel::Sentinels;

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    timeout(Duration::from_secs(20), async {
        // Set up the proxy CA — keep the TempDir alive for the duration.
        let (ca_setup, _ca_temp_dir) = ca::setup().expect("proxy CA setup");
        let ca_cert_pem =
            std::fs::read_to_string(&ca_setup.cert_pem_path).expect("read CA cert PEM");

        // Generate a self-signed server cert for "localhost".
        let server_cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("gen server cert");
        let server_cert_der = server_cert.cert.der().to_vec();
        let server_key_der = server_cert.signing_key.serialize_der();

        // Spawn an HTTPS server using the self-signed cert.
        let (server_port, _server_shutdown) = spawn_https_server(server_cert_der, server_key_der)
            .await
            .expect("spawn HTTPS server");

        // Generate sentinels and pre-compute the sentinel-backed header value.
        let sentinels = Arc::new(Sentinels::generate());
        let sentinel_value = format!("ghs_{}", sentinels.base64_key);

        // Compile the jq filter that checks the sentinel jaq variable directly.
        let filter_expr =
            r#"if .headers.authorization == "Bearer ghs_\($fake_base64_key)" then .headers.authorization = "Bearer real_token_from_env" else . end"#;
        let compiled = filter::compile(filter_expr).expect("compile filter");

        // Start the proxy with danger_accept_invalid_certs so it can connect
        // upstream to our self-signed test server.
        let proxy_port =
            proxy::start_proxy_danger_accept_invalid_certs(compiled, ca_setup, jaq_vars(&sentinels))
            .await
            .expect("start proxy");

        sleep(Duration::from_millis(100)).await;

        // Build a reqwest client that routes HTTPS through the proxy (CONNECT
        // tunnel). `danger_accept_invalid_certs` is required because the
        // MITM leaf certs hudsucker generates have no Extended Key Usage.
        let proxy_ca = reqwest::tls::Certificate::from_pem(ca_cert_pem.as_bytes())
            .expect("parse proxy CA cert");

        let client = reqwest::Client::builder()
            .proxy(
                reqwest::Proxy::https(format!("http://127.0.0.1:{proxy_port}"))
                    .expect("build proxy"),
            )
            .add_root_certificate(proxy_ca)
            .danger_accept_invalid_certs(true)
            .build()
            .expect("build reqwest client");

        // Send a request with the sentinel token in Authorization header.
        let response = client
            .get(format!("https://localhost:{server_port}/"))
            .header("Authorization", format!("Bearer {sentinel_value}"))
            .send()
            .await
            .expect("send request");

        assert!(
            response.status().is_success(),
            "expected 200, got {}",
            response.status()
        );

        let body: Value = response.json().await.expect("parse JSON body");

        // The proxy filter should have replaced the sentinel token with the real token.
        assert_eq!(
            body.get("authorization").and_then(Value::as_str),
            Some("Bearer real_token_from_env"),
            "proxy did not replace sentinel token via $fake_base64_key. Full echoed headers: {body}"
        );
    })
    .await
    .expect("test timed out");
}
