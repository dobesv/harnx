//! Integration tests for harnx-proxy-auth.
//!
//! Spawns the proxy binary as a subprocess, routes plain HTTP requests through it
//! to a local test server, and verifies that configured headers are injected.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
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

        shutdown_proxy(&mut proxy).await;
        test_result.expect("integration flow")
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn persistent_jsonl_mode_has_no_readiness_preamble() {
    timeout(Duration::from_secs(15), async {
        let mut proxy = Command::new(proxy_binary_path())
            .env(
                harnx_core::hooks::HARNX_HOOK_PROTOCOL_ENV,
                harnx_core::hooks::HARNX_HOOK_PROTOCOL_JSONL,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn proxy");

        let request = br#"{"id":"probe","tool_input":{"env":{}}}
"#;
        proxy
            .stdin
            .as_mut()
            .expect("proxy stdin")
            .write_all(request)
            .await
            .expect("write hook request");

        let stdout = proxy.stdout.take().expect("proxy stdout");
        let line = BufReader::new(stdout)
            .lines()
            .next_line()
            .await
            .expect("read first stdout line")
            .expect("proxy response line");
        let response: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("first stdout line was not JSON: {line:?}: {error}"));

        assert_eq!(response["id"], "probe");
        assert!(
            response["hookSpecificOutput"]["toolInput"]["env"]["HTTP_PROXY"]
                .as_str()
                .is_some()
        );

        shutdown_proxy(&mut proxy).await;
    })
    .await
    .expect("test timed out");
}

struct ProxyReadiness {
    proxy_port: u16,
    // CA cert PEM (decoded from CA_CERT_PEM_B64 readiness line).
    // Available for HTTPS tests; unused in the plain-HTTP test.
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

async fn spawn_proxy(host_matcher: &str) -> Result<Child> {
    let proxy_bin = proxy_binary_path();
    // jaq filter: if the host matches, inject the test header with a hardcoded value
    let hook = format!(
        r#"if .host == "{host_matcher}" then .headers["x-test-header"] = "hello-world" else . end"#
    );
    let child = Command::new(&proxy_bin)
        .arg("--hook")
        .arg(&hook)
        .stdin(Stdio::piped()) // Keep stdin open so the JSONL loop doesn't exit on EOF
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn proxy binary at {}", proxy_bin.display()))?;
    Ok(child)
}

/// Read lines from the proxy stdout until both `PROXY_PORT` and `CA_CERT_PEM_B64` are found.
/// Separated from `read_proxy_readiness` to keep cyclomatic complexity below threshold.
async fn collect_readiness_lines(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> Result<(u16, String)> {
    let mut proxy_port = None;
    let mut ca_cert_pem_b64 = None;

    for _ in 0..10 {
        let line = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .context("timed out waiting for proxy readiness output")??
            .ok_or_else(|| anyhow!("proxy exited before readiness output"))?;

        parse_readiness_line(&line, &mut proxy_port, &mut ca_cert_pem_b64)?;

        if proxy_port.is_some() && ca_cert_pem_b64.is_some() {
            break;
        }
    }

    Ok((
        proxy_port.ok_or_else(|| anyhow!("missing PROXY_PORT output"))?,
        ca_cert_pem_b64.ok_or_else(|| anyhow!("missing CA_CERT_PEM_B64 output"))?,
    ))
}

fn parse_readiness_line(
    line: &str,
    proxy_port: &mut Option<u16>,
    ca_cert_pem_b64: &mut Option<String>,
) -> Result<()> {
    if let Some(port) = line.strip_prefix("PROXY_PORT=") {
        *proxy_port = Some(port.parse::<u16>().context("invalid proxy port")?);
    } else if let Some(b64) = line.strip_prefix("CA_CERT_PEM_B64=") {
        *ca_cert_pem_b64 = Some(b64.to_string());
    }
    // CA_CERT_PATH line is also emitted; not needed here.
    Ok(())
}

async fn read_proxy_readiness(proxy: &mut Child) -> Result<ProxyReadiness> {
    let stdout = proxy
        .stdout
        .take()
        .ok_or_else(|| anyhow!("proxy stdout not captured"))?;
    let mut lines = BufReader::new(stdout).lines();

    let (proxy_port, ca_cert_b64) = collect_readiness_lines(&mut lines).await?;
    let ca_cert_pem = harnx_proxy_auth::base64_decode(&ca_cert_b64)
        .context("decode CA_CERT_PEM_B64 readiness line")?;

    Ok(ProxyReadiness {
        proxy_port,
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

        shutdown_proxy(&mut proxy).await;
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
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }

    let binary_name = if cfg!(windows) {
        "harnx-proxy-auth.exe"
    } else {
        "harnx-proxy-auth"
    };

    path.push(binary_name);
    path
}

async fn shutdown_proxy(proxy: &mut Child) {
    if proxy.id().is_none() {
        return;
    }

    let _ = proxy.start_kill();
    let _ = timeout(Duration::from_secs(3), proxy.wait()).await;
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
