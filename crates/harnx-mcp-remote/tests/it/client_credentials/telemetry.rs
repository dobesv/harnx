//! Exercises the binary's OTLP policy over stdio. Unix SIGTERM lets the real
//! exporter drain instead of killing the child when the stdio transport drops.

use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{body::Bytes, extract::State, http::StatusCode, routing::post, Router};
use opentelemetry_proto::tonic::{
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{any_value::Value, KeyValue},
    trace::v1::Span,
};
use prost::Message as _;
use rmcp::service::ClientLifecycleMode;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::process::{Child, Command};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use super::boundaries::HOSTILE_LOGGING;
use super::fixture::{
    call_tool_args, oauth_args, write_client_secret_file, ClientCredentialsUpstream, CLIENT_SECRET,
    SAFE_UPSTREAM_FAILURE, TOKEN_PREFIX,
};
use crate::common::{capture_stderr, proxy_binary_path, stderr_wait_eof, TestClientHandler};

const DEADLINE: Duration = Duration::from_secs(45);
const RUST_LOG: &str = concat!(
    "trace,rmcp::service=trace,rmcp::service[serve_inner]=trace,",
    "reqwest::connect=trace,hyper::proto=trace,hyper_util::client::legacy=trace",
);
const ECHO_PROSE: &str = "upstream rejected: saw bearer";

// Keep this independent of the binary's policy so a removed target isn't
// silently removed from the assertions too.
const FORBIDDEN_TARGETS: &[&str] = &["rmcp", "reqwest", "hyper", "hyper_util"];

type TraceCapture = Arc<Mutex<Vec<ExportTraceServiceRequest>>>;

struct TraceCollector {
    endpoint: String,
    capture: TraceCapture,
    stop: CancellationToken,
    task: AbortOnDropHandle<std::io::Result<()>>,
}

impl TraceCollector {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/v1/traces", listener.local_addr()?);
        let capture = TraceCapture::default();
        let router = Router::new()
            .route("/v1/traces", post(collect_traces))
            .with_state(capture.clone());
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        }));
        Ok(Self {
            endpoint,
            capture,
            stop,
            task,
        })
    }

    async fn drain(self) -> Result<Vec<ExportTraceServiceRequest>> {
        // After child exit no more exports can start. Join the collector so
        // every accepted request is included in the assertions.
        self.stop.cancel();
        tokio::time::timeout(DEADLINE, self.task)
            .await
            .context("collector did not drain")???;
        let mut capture = self.capture.lock().expect("trace capture lock");
        Ok(std::mem::take(&mut *capture))
    }
}

async fn collect_traces(State(capture): State<TraceCapture>, body: Bytes) -> StatusCode {
    let Ok(export) = ExportTraceServiceRequest::decode(body) else {
        return StatusCode::BAD_REQUEST;
    };
    capture.lock().expect("trace capture lock").push(export);
    StatusCode::OK
}

fn spawn_traced_proxy(args: &[String], endpoint: &str) -> Result<Child> {
    let binary = proxy_binary_path().context("harnx-mcp-remote binary not found")?;
    Command::new(binary)
        .args(args)
        .env_clear()
        .envs(HOSTILE_LOGGING.iter().copied())
        .env("RUST_LOG", RUST_LOG)
        .env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", endpoint)
        .env("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/protobuf")
        .env("OTEL_TRACES_SAMPLER", "always_on")
        // Require shutdown to flush; don't wait for a periodic export.
        .env("OTEL_BSP_SCHEDULE_DELAY", "3600000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn harnx-mcp-remote")
}

async fn signal_shutdown(child: &Child) -> Result<()> {
    let pid = child.id().context("proxy exited before SIGTERM")?;
    let delivered = Command::new("kill")
        .args(["-s", "TERM", &pid.to_string()])
        .status()
        .await?;
    anyhow::ensure!(delivered.success(), "kill -s TERM failed: {delivered}");
    Ok(())
}

async fn run_credential_echo(args: &[String], endpoint: &str) -> Result<(String, String)> {
    let mut child = spawn_traced_proxy(args, endpoint)?;
    let stderr = capture_stderr(child.stderr.take().context("proxy stderr")?);
    let transport = AsyncRwTransport::new_client(
        child.stdout.take().context("proxy stdout")?,
        child.stdin.take().context("proxy stdin")?,
    );
    let client = rmcp::service::serve_client_with_lifecycle(
        TestClientHandler,
        transport,
        ClientLifecycleMode::Initialize,
    )
    .await
    .context("initialize OAuth proxy")?;
    let error = client.call_tool(call_tool_args("fail_jsonrpc")).await;

    // Let the proxy handle SIGTERM and exit successfully before reading OTLP.
    // Dropping TokioChildProcess first would force-kill it after a grace period.
    signal_shutdown(&child).await?;
    let status = child.wait().await?;
    anyhow::ensure!(status.success(), "proxy exit {status}: {}", stderr.text());
    client.waiting().await?;
    anyhow::ensure!(
        stderr_wait_eof(&stderr, DEADLINE).await,
        "stderr did not reach EOF"
    );

    let Err(rmcp::ServiceError::McpError(frame)) = error else {
        anyhow::bail!("expected decoded JSON-RPC error, got {error:?}");
    };
    assert_eq!(frame.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    assert_eq!(frame.message, SAFE_UPSTREAM_FAILURE);
    Ok((serde_json::to_string(&frame)?, stderr.text()))
}

fn assert_allowed_targets(location: &str, attributes: &[KeyValue]) {
    for attribute in attributes.iter().filter(|attr| attr.key == "target") {
        let Some(Value::StringValue(target)) =
            attribute.value.as_ref().and_then(|v| v.value.as_ref())
        else {
            panic!("non-string target in OTLP {location}");
        };
        for root in FORBIDDEN_TARGETS {
            assert!(
                target != root
                    && !target
                        .strip_prefix(root)
                        .is_some_and(|tail| tail.starts_with("::")),
                "forbidden OTLP target {target:?} on {location}"
            );
        }
    }
}

fn assert_exported_spans_are_safe(exports: &[ExportTraceServiceRequest]) {
    let spans: Vec<&Span> = exports
        .iter()
        .flat_map(|batch| &batch.resource_spans)
        .flat_map(|resource| &resource.scope_spans)
        .flat_map(|scope| &scope.spans)
        .collect();
    assert!(
        spans
            .iter()
            .any(|span| span.name == "client_credentials_proxy_started"),
        "OAuth startup span did not arrive at the collector"
    );
    for span in spans {
        assert_allowed_targets(&span.name, &span.attributes);
        for event in &span.events {
            assert_allowed_targets(&event.name, &event.attributes);
        }
    }
}

fn assert_no_echo(surface: &str, text: &str) {
    for sentinel in [CLIENT_SECRET, TOKEN_PREFIX, ECHO_PROSE, "fail_jsonrpc"] {
        assert!(
            !text.contains(sentinel),
            "{surface} contains forbidden marker {sentinel:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_child_drains_safe_otlp_without_dependency_payloads() -> Result<()> {
    harnx_core::require_nextest();
    let upstream = ClientCredentialsUpstream::spawn().await;
    let (_dir, secret) = write_client_secret_file();
    let collector = TraceCollector::start().await?;
    let args = oauth_args(&upstream, &secret, &[]);
    let (response, stderr) =
        tokio::time::timeout(DEADLINE, run_credential_echo(&args, &collector.endpoint))
            .await
            .context("OAuth proxy exchange or graceful shutdown timed out")??;
    assert_eq!(upstream.state.token_exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(upstream.state.unauthorized_401s.load(Ordering::SeqCst), 0);
    upstream.shutdown().await;
    let exports = collector.drain().await?;

    assert!(stderr.contains("starting in client-credentials mode"));
    assert_no_echo("stdio response", &response);
    assert_no_echo("drained stderr", &stderr);
    // Inspect payload fields throughout the protobuf, including span status,
    // event fields, resource attributes, and instrumentation scope fields.
    assert_no_echo("OTLP export", &format!("{exports:?}"));
    assert_exported_spans_are_safe(&exports);
    Ok(())
}
