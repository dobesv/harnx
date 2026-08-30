//! Opt-in Prometheus metrics facade for harnx services and tool servers.

use std::{
    net::SocketAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context};
use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use clap::Args;
use metrics_exporter_prometheus::PrometheusBuilder;

/// Total chat-completion tokens by agent, client, model, and token type.
pub const LLM_TOKENS_TOTAL: &str = "harnx_llm_tokens_total";
/// Cumulative chat-completion cost in dollars by agent, client, and model.
pub const LLM_COST_DOLLARS: &str = "harnx_llm_cost_dollars";
/// Total HTTP requests by method, route, and status.
pub const HTTP_REQUESTS_TOTAL: &str = "harnx_http_requests_total";
/// HTTP request duration in seconds by method and route.
pub const HTTP_REQUEST_DURATION_SECONDS: &str = "harnx_http_request_duration_seconds";
/// Total tool calls by tool and status.
pub const TOOL_CALLS_TOTAL: &str = "harnx_tool_calls_total";
/// Tool-call duration in seconds by tool.
pub const TOOL_CALL_DURATION_SECONDS: &str = "harnx_tool_call_duration_seconds";

const HISTOGRAM_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

static INIT: OnceLock<()> = OnceLock::new();
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Command-line configuration for the optional Prometheus listener.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct MetricsFlags {
    #[arg(
        long,
        value_name = "ADDR",
        help = "Serve Prometheus metrics at http://ADDR/metrics. Blank host binds 0.0.0.0, e.g. :8456. Unset disables."
    )]
    pub metrics_addr: Option<String>,
}

/// Extract a metrics listener address from command-line arguments.
///
/// Accepts both `--metrics-addr ADDR` and `--metrics-addr=ADDR`.
pub fn metrics_addr_from_args<I>(args: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--metrics-addr" {
            return args.next();
        }
        if let Some(addr) = arg.strip_prefix("--metrics-addr=") {
            return Some(addr.to_owned());
        }
    }
    None
}

/// Initialize the Prometheus metrics recorder and HTTP listener.
///
/// If `metrics_addr` is `None`, this is a no-op: no recorder is installed and
/// `metrics::` macros emit nothing. Call this once at binary startup when
/// `--metrics-addr` is set.
///
/// Idempotent: guarded by a `OnceLock`. Second and subsequent calls are no-ops
/// (PrometheusBuilder::install would error on a second global-recorder set).
/// Address validation happens before the idempotency gate so invalid addresses
/// are rejected even on repeat calls.
pub fn init(flags: &MetricsFlags) -> anyhow::Result<()> {
    let Some(raw_addr) = flags.metrics_addr.as_deref() else {
        return Ok(());
    };
    let addr = parse_addr(raw_addr)?;

    if INIT.get().is_some() {
        return Ok(());
    }

    let _guard = INSTALL_LOCK
        .lock()
        .map_err(|_| anyhow!("metrics initialization lock is poisoned"))?;
    if INIT.get().is_some() {
        return Ok(());
    }

    PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets(&HISTOGRAM_BUCKETS)
        .context("failed to configure Prometheus histogram buckets")?
        .install()
        .context("failed to install Prometheus metrics recorder")?;
    INIT.set(())
        .map_err(|_| anyhow!("metrics recorder was initialized concurrently"))?;
    tracing::info!(%addr, "Prometheus metrics listener started");

    Ok(())
}

fn parse_addr(raw_addr: &str) -> anyhow::Result<SocketAddr> {
    let normalized = raw_addr
        .strip_prefix(':')
        .map(|port| format!("0.0.0.0:{port}"))
        .unwrap_or_else(|| raw_addr.to_owned());

    normalized
        .parse()
        .with_context(|| format!("invalid metrics address `{raw_addr}`; expected IP:PORT or :PORT"))
}

fn normalized_http_method(method: &str) -> &str {
    match method {
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "CONNECT" | "OPTIONS" | "TRACE" | "PATCH" => {
            method
        }
        _ => "other",
    }
}

/// Record request count and duration using a caller-supplied bounded route label.
pub fn record_http_request(method: &str, route: &str, status: u16, elapsed: Duration) {
    let method = normalized_http_method(method);
    metrics::counter!(
        HTTP_REQUESTS_TOTAL,
        "method" => method.to_owned(),
        "route" => route.to_owned(),
        "status" => status.to_string(),
    )
    .increment(1);
    metrics::histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        "method" => method.to_owned(),
        "route" => route.to_owned(),
    )
    .record(elapsed.as_secs_f64());
}

/// Record axum request count and duration, labeling routes by their matched template.
///
/// The `route` label comes from axum `MatchedPath` (path template) or a fixed literal,
/// never from `uri().path()` (unbounded cardinality). Do not add `session_id` or
/// other high-cardinality labels.
pub async fn http_metrics_middleware(req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_owned();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let started = Instant::now();

    let response = next.run(req).await;

    record_http_request(
        &method,
        &route,
        response.status().as_u16(),
        started.elapsed(),
    );
    response
}

/// Record a tool call completion with its duration and status.
///
/// Call this after a tool invocation completes to emit metrics:
/// - `harnx_tool_calls_total{tool,status}` counter incremented by 1
/// - `harnx_tool_call_duration_seconds{tool}` histogram observation
///
/// # Arguments
/// * `tool` - The tool name (used as the `tool` label). Should be a fixed, bounded identifier.
/// * `is_ok` - `true` for success, `false` for error (used as the `status` label).
/// * `elapsed` - The elapsed time of the tool invocation.
///
/// This is a no-op if no metrics recorder has been installed.
pub fn record_tool_call(tool: &str, is_ok: bool, elapsed: Duration) {
    let status = if is_ok { "ok" } else { "error" };
    metrics::counter!(TOOL_CALLS_TOTAL, "tool" => tool.to_string(), "status" => status)
        .increment(1);
    metrics::histogram!(TOOL_CALL_DURATION_SECONDS, "tool" => tool.to_string())
        .record(elapsed.as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
        middleware,
        routing::get,
        Router,
    };
    use metrics::{Key, Label};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey, MetricKind,
    };
    use tower::ServiceExt as _;

    fn metric_key(kind: MetricKind, name: &'static str, labels: &[(&str, &str)]) -> CompositeKey {
        CompositeKey::new(
            kind,
            Key::from_parts(
                name,
                labels
                    .iter()
                    .map(|(key, value)| Label::new((*key).to_owned(), (*value).to_owned()))
                    .collect::<Vec<_>>(),
            ),
        )
    }

    #[test]
    fn http_helper_records_count_and_duration() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let elapsed = Duration::from_millis(125);

        metrics::with_local_recorder(&recorder, || {
            record_http_request("POST", "/v1/embeddings", 202, elapsed);
        });

        assert_eq!(
            snapshotter.snapshot().into_vec(),
            vec![
                (
                    metric_key(
                        MetricKind::Counter,
                        HTTP_REQUESTS_TOTAL,
                        &[
                            ("method", "POST"),
                            ("route", "/v1/embeddings"),
                            ("status", "202"),
                        ],
                    ),
                    None,
                    None,
                    DebugValue::Counter(1),
                ),
                (
                    metric_key(
                        MetricKind::Histogram,
                        HTTP_REQUEST_DURATION_SECONDS,
                        &[("method", "POST"), ("route", "/v1/embeddings")],
                    ),
                    None,
                    None,
                    DebugValue::Histogram(vec![elapsed.as_secs_f64().into()]),
                ),
            ]
        );
    }

    #[test]
    fn http_method_labels_are_normalized() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            record_http_request("FROBNICATE", "/custom", 200, Duration::from_millis(1));
            record_http_request("GET", "/standard", 200, Duration::from_millis(1));
        });

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(snapshot.len(), 4);
        for (method, route) in [("other", "/custom"), ("GET", "/standard")] {
            assert!(snapshot.iter().any(|(key, _, _, value)| {
                *key == metric_key(
                    MetricKind::Counter,
                    HTTP_REQUESTS_TOTAL,
                    &[("method", method), ("route", route), ("status", "200")],
                ) && *value == DebugValue::Counter(1)
            }));
            assert!(snapshot.iter().any(|(key, _, _, value)| {
                *key == metric_key(
                    MetricKind::Histogram,
                    HTTP_REQUEST_DURATION_SECONDS,
                    &[("method", method), ("route", route)],
                ) && matches!(value, DebugValue::Histogram(samples) if samples.len() == 1)
            }));
        }
        assert!(snapshot.iter().all(|(key, _, _, _)| {
            key.key()
                .labels()
                .all(|label| label.value() != "FROBNICATE")
        }));
    }

    #[test]
    fn http_middleware_uses_matched_route_template() {
        let app = Router::new()
            .route("/v1/models/{id}", get(|| async { StatusCode::CREATED }))
            .layer(middleware::from_fn(http_metrics_middleware));
        let request = HttpRequest::builder()
            .uri("/v1/models/customer-42?detail=full")
            .body(Body::empty())
            .expect("request should build");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime should build");
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        let response =
            metrics::with_local_recorder(&recorder, || runtime.block_on(app.oneshot(request)))
                .expect("request should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(snapshot.len(), 2);
        let counter = snapshot
            .iter()
            .find(|(key, _, _, _)| key.kind() == MetricKind::Counter)
            .expect("request counter should be recorded");
        assert_eq!(
            counter.0,
            metric_key(
                MetricKind::Counter,
                HTTP_REQUESTS_TOTAL,
                &[
                    ("method", "GET"),
                    ("route", "/v1/models/{id}"),
                    ("status", "201"),
                ],
            )
        );
        assert_eq!(counter.3, DebugValue::Counter(1));

        let histogram = snapshot
            .iter()
            .find(|(key, _, _, _)| key.kind() == MetricKind::Histogram)
            .expect("request duration should be recorded");
        assert_eq!(
            histogram.0,
            metric_key(
                MetricKind::Histogram,
                HTTP_REQUEST_DURATION_SECONDS,
                &[("method", "GET"), ("route", "/v1/models/{id}")],
            )
        );
        match &histogram.3 {
            DebugValue::Histogram(observations) => assert_eq!(observations.len(), 1),
            value => panic!("expected histogram observation, got {value:?}"),
        }
    }

    #[test]
    fn blank_host_binds_all_interfaces() {
        assert_eq!(
            parse_addr(":8456").expect("blank-host address should parse"),
            "0.0.0.0:8456".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn metrics_addr_parses_separate_and_equals_forms() {
        assert_eq!(
            metrics_addr_from_args([
                "harnx-tool".to_owned(),
                "--metrics-addr".to_owned(),
                ":8456".to_owned(),
            ]),
            Some(":8456".to_owned())
        );
        assert_eq!(
            metrics_addr_from_args([
                "harnx-tool".to_owned(),
                "--metrics-addr=127.0.0.1:8456".to_owned(),
            ]),
            Some("127.0.0.1:8456".to_owned())
        );
    }

    #[test]
    fn host_and_port_parse_as_socket_address() {
        assert_eq!(
            parse_addr("127.0.0.1:8456").expect("socket address should parse"),
            "127.0.0.1:8456".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn invalid_address_is_rejected() {
        let error = parse_addr("not-an-address").expect_err("invalid address should fail");
        assert!(error.to_string().contains("invalid metrics address"));
    }

    #[test]
    fn disabled_metrics_are_a_noop() {
        init(&MetricsFlags { metrics_addr: None }).expect("disabled metrics should be a no-op");
    }

    #[test]
    fn installation_is_idempotent() {
        init(&MetricsFlags {
            metrics_addr: Some(":0".to_owned()),
        })
        .expect("blank-host listener should install");
        init(&MetricsFlags {
            metrics_addr: Some("127.0.0.1:0".to_owned()),
        })
        .expect("repeat initialization should be a no-op");

        let error = init(&MetricsFlags {
            metrics_addr: Some("invalid".to_owned()),
        })
        .expect_err("addresses should be validated before the idempotency gate");
        assert!(error.to_string().contains("invalid metrics address"));
    }
}
