#![cfg(unix)]

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use harnx_proxy_auth::filter::{compile_with_vars, JaqVars};
use harnx_proxy_auth::sentinel::Sentinels;
use harnx_proxy_auth::transform::{Stage, TransformPipeline};
use harnx_proxy_auth::{ca, proxy};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout, Duration};
use tokio_rustls::rustls::{self, pki_types};
use tokio_rustls::TlsAcceptor;

fn jaq_vars() -> Arc<JaqVars> {
    Arc::new(JaqVars::new(&Sentinels::generate(), String::new(), None, vec![]).unwrap())
}

fn jaq_stage(expr: &str, vars: &Arc<JaqVars>) -> Stage {
    Stage::Jaq {
        filter: Arc::new(compile_with_vars(expr, vars).unwrap()),
        vars: vars.clone(),
    }
}

fn exec_stage(script: &str, timeout_secs: u64) -> Stage {
    Stage::Exec(Arc::new(
        harnx_proxy_auth::exec_hook::ExecHookProcess::spawn_inline(script, timeout_secs).unwrap(),
    ))
}

/// Starts an HTTPS test server using provided cert+key (DER-encoded).
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
                        let svc = service_fn(echo_request);
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

async fn echo_request(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let headers: BTreeMap<String, String> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = serde_json::json!({
        "path": req.uri().path_and_query().map_or("/", |pq| pq.as_str()),
        "headers": headers,
    });
    let body = serde_json::to_vec(&body).unwrap_or_default();
    Ok(Response::builder()
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

async fn send_https_request(pipeline: TransformPipeline) -> Result<Value> {
    let (ca_setup, _ca_temp_dir) = ca::setup().context("proxy CA setup")?;
    let ca_cert_pem =
        std::fs::read_to_string(&ca_setup.cert_pem_path).context("read proxy CA cert PEM")?;

    let server_cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .context("generate self-signed server cert")?;
    let server_cert_der = server_cert.cert.der().to_vec();
    let server_key_der = server_cert.signing_key.serialize_der();

    let (server_port, _server_shutdown) = spawn_https_server(server_cert_der, server_key_der)
        .await
        .context("spawn HTTPS server")?;

    let proxy_port = proxy::start_proxy_danger_accept_invalid_certs(Arc::new(pipeline), ca_setup)
        .await
        .context("start proxy")?;

    sleep(Duration::from_millis(100)).await;

    let proxy_ca = reqwest::tls::Certificate::from_pem(ca_cert_pem.as_bytes())
        .context("parse proxy CA cert")?;

    let client = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::https(format!("http://127.0.0.1:{proxy_port}"))
                .context("build proxy")?,
        )
        .add_root_certificate(proxy_ca)
        .danger_accept_invalid_certs(true)
        .build()
        .context("build reqwest client")?;

    let response = client
        .get(format!("https://localhost:{server_port}/hook-test"))
        .send()
        .await
        .context("send request")?;

    assert!(
        response.status().is_success(),
        "expected 200, got {}",
        response.status()
    );

    response.json().await.context("parse echoed JSON body")
}

#[tokio::test]
async fn mixed_jaq_and_shebang_hooks_apply_in_cli_order() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    timeout(Duration::from_secs(30), async {
        let vars = jaq_vars();
        let pipeline = TransformPipeline::new(vec![
            jaq_stage(
                r#".headers["x-order"] = "A" | .headers["x-marker"] = "jaq1""#,
                &vars,
            ),
            exec_stage(
                r##"#!/bin/sh
printf 'READY
'
while IFS= read -r line; do
  HOOK_LINE="$line" python3 - <<'PY'
import json
import os
msg = json.loads(os.environ["HOOK_LINE"])
headers = msg.setdefault("headers", {})
headers["x-order"] = headers.get("x-order", "") + "E1"
headers["x-marker"] = headers.get("x-marker", "") + "-exec1"
print(json.dumps(msg), flush=True)
PY
done
"##,
                5,
            ),
            jaq_stage(
                r#".headers["x-order"] += "B" | .headers["x-marker"] += "-jaq2""#,
                &vars,
            ),
        ]);

        let body = send_https_request(pipeline)
            .await
            .expect("round-trip request");
        let headers = body
            .get("headers")
            .and_then(Value::as_object)
            .expect("headers object");

        assert_eq!(
            headers.get("x-order").and_then(Value::as_str),
            Some("AE1B"),
            "stages did not apply in CLI order. Full body: {body}"
        );
        assert_eq!(
            headers.get("x-marker").and_then(Value::as_str),
            Some("jaq1-exec1-jaq2"),
            "marker header did not preserve jaq -> exec -> jaq sequencing. Full body: {body}"
        );
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
async fn two_shebang_hooks_both_run() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    timeout(Duration::from_secs(30), async {
        let pipeline = TransformPipeline::new(vec![
            exec_stage(
                r##"#!/bin/sh
printf 'READY
'
while IFS= read -r line; do
  HOOK_LINE="$line" python3 - <<'PY'
import json
import os
msg = json.loads(os.environ["HOOK_LINE"])
headers = msg.setdefault("headers", {})
headers["x-chain"] = headers.get("x-chain", "") + "1"
print(json.dumps(msg), flush=True)
PY
done
"##,
                5,
            ),
            exec_stage(
                r##"#!/bin/sh
printf 'READY
'
while IFS= read -r line; do
  HOOK_LINE="$line" python3 - <<'PY'
import json
import os
msg = json.loads(os.environ["HOOK_LINE"])
headers = msg.setdefault("headers", {})
headers["x-chain"] = headers.get("x-chain", "") + "2"
print(json.dumps(msg), flush=True)
PY
done
"##,
                5,
            ),
        ]);

        let body = send_https_request(pipeline)
            .await
            .expect("round-trip request");
        let headers = body
            .get("headers")
            .and_then(Value::as_object)
            .expect("headers object");

        assert_eq!(
            headers.get("x-chain").and_then(Value::as_str),
            Some("12"),
            "both exec stages should run in order. Full body: {body}"
        );
    })
    .await
    .expect("test timed out");
}
