//! Integration tests for harnx-proxy-auth.
//!
//! Spawns the proxy binary as a subprocess, routes plain HTTP requests through it
//! to a local test server, and verifies that configured headers are injected.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn test_header_injection_through_proxy() {
    timeout(Duration::from_secs(15), async {
        let mut proxy = spawn_proxy("localhost").await.expect("spawn proxy");

        let test_result = async {
            let readiness = read_proxy_readiness(&mut proxy).await?;
            sleep(Duration::from_millis(200)).await;

            let server = spawn_test_server().await?;

            // Plain HTTP through HTTP proxy — no TLS needed, no cert validation.
            // The proxy's handle_request fires for plain HTTP requests too.
            let client = reqwest::Client::builder()
                .proxy(reqwest::Proxy::http(format!(
                    "http://127.0.0.1:{}",
                    readiness.proxy_port
                ))?)
                .build()?;

            let response = client
                .get(format!("http://localhost:{}/", server.port))
                .send()
                .await?;

            let status = response.status();
            let body_bytes = response.bytes().await?;
            let body_str = String::from_utf8_lossy(&body_bytes);

            if !status.is_success() {
                return Err(anyhow!(
                    "proxy request failed: status={status}, body={body_str}"
                ));
            }
            if body_bytes.is_empty() {
                return Err(anyhow!("test server returned empty body (status={status})"));
            }
            let body: Value = serde_json::from_slice(&body_bytes)
                .map_err(|e| anyhow!("JSON parse error: {e}, body={body_str:?}"))?;

            let injected = body
                .get("x-test-header")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing x-test-header in echoed headers: {body}"))?;

            assert_eq!(injected, "hello-world");
            Ok::<(), anyhow::Error>(())
        }
        .await;

        shutdown_proxy(proxy).await;
        test_result.expect("integration flow")
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn ca_cert_file_persists_while_proxy_runs() {
    timeout(Duration::from_secs(15), async {
        let mut proxy = spawn_proxy("localhost").await.expect("spawn proxy");

        let test_result = async {
            let readiness = read_proxy_readiness(&mut proxy).await?;
            sleep(Duration::from_millis(200)).await;

            // Regression test for #1622: the CA temp dir must outlive build_runtime.
            let cert_pem = std::fs::read_to_string(&readiness.ca_cert_path).with_context(|| {
                format!(
                    "read CA certificate at {} while proxy is running",
                    readiness.ca_cert_path.display()
                )
            })?;
            if cert_pem.is_empty() {
                return Err(anyhow!("CA certificate file is empty"));
            }
            if !cert_pem.contains("-----BEGIN CERTIFICATE-----") {
                return Err(anyhow!(
                    "CA certificate file does not contain a PEM certificate"
                ));
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        shutdown_proxy(proxy).await;
        test_result.expect("CA certificate should persist for proxy lifetime");
    })
    .await
    .expect("test timed out");
}

struct ProxyReadiness {
    proxy_port: u16,
    ca_cert_path: PathBuf,
    #[allow(dead_code)]
    ca_cert_pem: Vec<u8>,
}

struct TestServer {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn spawn_proxy(host_matcher: &str) -> Result<super::mcp_stdio::Session> {
    let proxy_bin = proxy_binary_path();
    let hook = format!(
        r#"if .host == "{host_matcher}" then .headers["x-test-header"] = "hello-world" else . end"#
    );
    let mut command = Command::new(&proxy_bin);
    command
        .arg("--mcp-stdio")
        .arg("--name=proxy_fixture")
        .arg("--hook")
        .arg(&hook);
    super::mcp_stdio::Session::start(&mut command, "proxy_fixture", false).await
}

async fn read_proxy_readiness(session: &mut super::mcp_stdio::Session) -> Result<ProxyReadiness> {
    let env_output = session.environment().await?;
    let http_proxy = env_output["env"]["HTTP_PROXY"]
        .as_str()
        .ok_or_else(|| anyhow!("missing HTTP_PROXY"))?;
    let proxy_port = http_proxy
        .rsplit(':')
        .next()
        .ok_or_else(|| anyhow!("invalid HTTP_PROXY port"))?
        .parse::<u16>()?;
    let ca_cert_path = PathBuf::from(
        env_output["filesystem"]["ca_cert_path"]
            .as_str()
            .ok_or_else(|| anyhow!("missing ca_cert_path"))?,
    );
    let ca_cert_pem = std::fs::read(&ca_cert_path).unwrap_or_default();
    Ok(ProxyReadiness {
        proxy_port,
        ca_cert_path,
        ca_cert_pem,
    })
}

async fn spawn_test_server() -> Result<TestServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accept_result = listener.accept() => {
                    let Ok((stream, _addr)) = accept_result else { break; };
                    tokio::spawn(async move {
                        let service = service_fn(handle_test_request);
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
    });

    Ok(TestServer {
        port,
        shutdown: Some(shutdown_tx),
    })
}

async fn handle_test_request(
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let headers = req
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let body = serde_json::to_vec(&headers).expect("serialize headers");
    let response = Response::builder()
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("build response");
    Ok(response)
}

/// NOTE: The HTTPS CONNECT tunnel regression test lives in tests/https_connect.rs
/// (header_injection_works_through_https_connect_tunnel). It uses the proxy
/// library directly so it can configure a custom outbound TLS connector for
/// the self-signed test server cert. The binary-based test below is removed
/// because the binary's outbound connector cannot be reconfigured for test certs.
#[tokio::test]
#[ignore = "superseded by tests/https_connect.rs which tests the same scenario via the library API"]
async fn test_header_injection_through_https_connect_tunnel() {
    timeout(Duration::from_secs(15), async {
        let mut proxy = spawn_proxy("localhost").await.expect("spawn proxy");

        let test_result = async {
            let readiness = read_proxy_readiness(&mut proxy).await?;
            sleep(Duration::from_millis(200)).await;

            // Spawn a TLS test server with a self-signed cert.
            let server = spawn_tls_test_server().await?;

            // Build a reqwest client that routes HTTPS through the proxy.
            // The proxy intercepts the TLS (MITM) and re-encrypts using its CA.
            // We trust the proxy CA for the client-side TLS.
            // We use danger_accept_invalid_hostnames=false but need the proxy CA
            // trusted; the proxy's outbound connector handles upstream TLS separately.
            let proxy_ca = reqwest::tls::Certificate::from_pem(&readiness.ca_cert_pem)
                .context("parse proxy CA cert")?;

            let client = reqwest::Client::builder()
                .proxy(reqwest::Proxy::https(format!(
                    "http://127.0.0.1:{}",
                    readiness.proxy_port
                ))?)
                .add_root_certificate(proxy_ca)
                // The proxy's outbound TLS connector sees our self-signed server
                // cert; accept it so the proxy can forward the request upstream.
                .danger_accept_invalid_certs(true)
                .build()?;

            let response = client
                .get(format!("https://localhost:{}/", server.port))
                .send()
                .await?;

            let status = response.status();
            let body_bytes = response.bytes().await?;
            let body_str = String::from_utf8_lossy(&body_bytes);

            if !status.is_success() {
                return Err(anyhow!(
                    "proxy HTTPS request failed: status={status}, body={body_str}"
                ));
            }
            let body: Value = serde_json::from_slice(&body_bytes)
                .map_err(|e| anyhow!("JSON parse error: {e}, body={body_str:?}"))?;

            // The proxy hook matches `.host == "localhost"` and injects
            // `x-test-header`. If host resolution from the Host header is
            // broken, the header won't be present.
            let injected = body
                .get("x-test-header")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!(
                        "missing x-test-header — proxy did not inject header for tunnelled HTTPS request. \
                         echoed headers: {body}"
                    )
                })?;

            assert_eq!(injected, "hello-world");
            Ok::<(), anyhow::Error>(())
        }
        .await;

        shutdown_proxy(proxy).await;
        test_result.expect("HTTPS CONNECT tunnel integration flow")
    })
    .await
    .expect("test timed out");
}

struct TlsTestServer {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Drop for TlsTestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn spawn_tls_test_server() -> Result<TlsTestServer> {
    use tokio_rustls::rustls::{self, pki_types};
    use tokio_rustls::TlsAcceptor;

    // Generate a fresh self-signed cert for "localhost".
    let server_key = rcgen::KeyPair::generate().context("generate server key")?;
    let mut server_params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .context("build server cert params")?;
    server_params.is_ca = rcgen::IsCa::NoCa;
    let server_cert = server_params
        .self_signed(&server_key)
        .context("self-sign server cert")?;
    let cert_der = pki_types::CertificateDer::from(server_cert.der().to_vec());
    let key_der = pki_types::PrivateKeyDer::try_from(server_key.serialize_der())
        .map_err(|e| anyhow::anyhow!("wrap server key: {e}"))?;

    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .context("build TLS server config")?;
    let acceptor = TlsAcceptor::from(std::sync::Arc::new(tls_config));

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accept_result = listener.accept() => {
                    let Ok((stream, _addr)) = accept_result else { break; };
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let Ok(tls_stream) = acceptor.accept(stream).await else { return; };
                        let service = service_fn(handle_test_request);
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(tls_stream), service)
                            .await;
                    });
                }
            }
        }
    });

    Ok(TlsTestServer {
        port,
        shutdown: Some(shutdown_tx),
    })
}

fn proxy_binary_path() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_harnx-proxy-auth") {
        return PathBuf::from(path);
    }

    let mut path = std::env::current_exe().expect("current test executable path");
    path = harnx_core::bin_dir_of_exe(&path);

    let binary_name = if cfg!(windows) {
        "harnx-proxy-auth.exe"
    } else {
        "harnx-proxy-auth"
    };

    path.push(binary_name);
    path
}

async fn shutdown_proxy(proxy: super::mcp_stdio::Session) {
    let _ = proxy.shutdown().await;
}

#[test]
fn proxy_binary_path_falls_back_to_target_debug() {
    let path = proxy_binary_path();
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    assert!(file_name.contains("harnx-proxy-auth"));
    assert!(path.is_absolute() || Path::new(&path).components().count() > 0);
}
