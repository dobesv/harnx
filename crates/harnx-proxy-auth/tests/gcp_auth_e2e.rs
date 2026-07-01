#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout, Duration};
use tokio_rustls::rustls::{self, pki_types};
use tokio_rustls::TlsAcceptor;

use harnx_proxy_auth::{
    ca,
    exec_hook::ExecHookProcess,
    proxy,
    transform::{Stage, TransformPipeline},
};

const GCP_HOOK_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../example_config/gcp-auth-hook.py"
);

fn gcp_hook_script() -> String {
    std::fs::read_to_string(GCP_HOOK_PATH).expect("read gcp auth hook script")
}

fn gcp_pipeline(timeout_secs: u64) -> Arc<TransformPipeline> {
    Arc::new(TransformPipeline::new(vec![Stage::Exec(Arc::new(
        ExecHookProcess::spawn_inline(&gcp_hook_script(), timeout_secs)
            .expect("spawn gcp exec hook"),
    ))]))
}

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

#[tokio::test]
async fn gcp_hook_rewrites_googleapis_auth_and_hides_real_token_from_metadata() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    timeout(Duration::from_secs(20), async {
        // Safe per-process under nextest: each test gets its own process env.
        unsafe {
            std::env::set_var("HARNX_GCP_TOKEN_CMD", "printf e2e-real-token");
        }

        let pipeline = gcp_pipeline(2);
        sleep(Duration::from_millis(50)).await;

        let metadata = pipeline
            .apply(json!({
                "host": "metadata.google.internal",
                "method": "GET",
                "path": "/computeMetadata/v1/instance/service-accounts/default/token",
                "headers": {"metadata-flavor": "Google"},
                "url": "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token"
            }))
            .await;

        assert_eq!(metadata["respond"]["status"], 200);
        assert_eq!(metadata["respond"]["headers"]["metadata-flavor"], "Google");
        assert_eq!(
            metadata["respond"]["headers"]["content-type"],
            "application/json"
        );
        let metadata_body = metadata["respond"]["body"]
            .as_str()
            .expect("metadata response body string");
        let metadata_json: Value = serde_json::from_str(metadata_body).expect("metadata body JSON");
        assert_eq!(metadata_json["access_token"], "proxy-managed");
        assert_eq!(metadata_json["token_type"], "Bearer");
        assert_eq!(metadata_json["expires_in"], 3600);
        assert!(
            !metadata_body.contains("e2e-real-token"),
            "synthetic metadata response leaked real token: {metadata_body}"
        );

        let transformed = pipeline
            .apply(json!({
                "host": "bigquery.googleapis.com",
                "method": "GET",
                "path": "/bigquery/v2/projects/test-project/datasets",
                "headers": {}
            }))
            .await;

        assert!(transformed.get("respond").is_none());
        assert_eq!(
            transformed["headers"]["authorization"],
            "Bearer e2e-real-token"
        );

        let (ca_setup, _ca_temp_dir) = ca::setup().expect("proxy CA setup");
        let ca_cert_pem =
            std::fs::read_to_string(&ca_setup.cert_pem_path).expect("read CA cert PEM");

        let server_cert =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("gen cert");
        let server_cert_der = server_cert.cert.der().to_vec();
        let server_key_der = server_cert.signing_key.serialize_der();
        let (server_port, _server_shutdown) = spawn_https_server(server_cert_der, server_key_der)
            .await
            .expect("spawn HTTPS server");

        let proxy_port = proxy::start_proxy_danger_accept_invalid_certs(pipeline.clone(), ca_setup)
            .await
            .expect("start proxy");
        sleep(Duration::from_millis(100)).await;

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
            .header("Authorization", "Bearer proxy-managed")
            .send()
            .await
            .expect("send proxied request");

        assert!(
            response.status().is_success(),
            "unexpected status {}",
            response.status()
        );
        let body: Value = response.json().await.expect("parse echoed headers");
        assert_eq!(
            body.get("authorization").and_then(Value::as_str),
            Some("Bearer proxy-managed"),
            "proxy did not forward injected header to upstream HTTPS request. Echoed headers: {body}"
        );
    })
    .await
    .expect("test timed out");
}
