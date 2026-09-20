//! OpenTelemetry tracing initialization for harnx services.
//!
//! This crate provides env-gated OTLP HTTP and gRPC trace export setup and W3C trace-context
//! propagation across harnx transport boundaries (HTTP, NATS, rmcp `_meta`).
//!
//! # Runtime requirements
//!
//! The batch span processor uses Tokio's async runtime. Call [`init_telemetry`]
//! from within a Tokio runtime context. The returned [`TelemetryGuard`] must
//! be `shutdown().await` before process exit to flush pending spans.
//!
//! # Feature flags
//!
//! - `nats`: Enables NATS header propagation helpers.
//! - `mcp`: Enables rmcp `CallToolRequestParams` propagation helpers.
//! - `testing`: In-memory span exporter for test assertions.

pub mod propagate;

use std::env;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::Result;
use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_http::{Bytes, HttpClient, HttpError};
use opentelemetry_otlp::{
    Protocol, SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

/// Default tracing filter: disables everything except `harnx` targets at info level.
///
/// **Gotcha:** Spans without a `harnx*` target prefix are filtered out. Tests emitting
/// spans for collection must set `target: "harnx_telemetry"` (or similar) or spans won't
/// be exported.
const DEFAULT_FILTER: &str = "off,harnx=info";
const OTLP_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const OTLP_TRACES_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT";
const OTLP_HEADERS: &str = "OTEL_EXPORTER_OTLP_HEADERS";
const OTLP_TRACES_HEADERS: &str = "OTEL_EXPORTER_OTLP_TRACES_HEADERS";
const OTLP_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_TIMEOUT";
const OTLP_TRACES_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT";
const OTEL_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
const DEFAULT_OTLP_TIMEOUT: Duration = Duration::from_secs(10);

/// Protocol selection env var (generic fallback for all signals).
const OTLP_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
/// Protocol selection env var (signal-specific, takes precedence over `OTLP_PROTOCOL`).
const OTLP_TRACES_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL";

const FORWARDED_OTEL_ENV: [&str; 9] = [
    OTLP_ENDPOINT,
    OTLP_TRACES_ENDPOINT,
    OTLP_PROTOCOL,
    OTLP_TRACES_PROTOCOL,
    OTLP_HEADERS,
    OTLP_TRACES_HEADERS,
    "OTEL_RESOURCE_ATTRIBUTES",
    "OTEL_TRACES_SAMPLER",
    "OTEL_TRACES_SAMPLER_ARG",
];

/// Internal representation of supported OTLP trace export protocols.
///
/// This is a harnx-internal enum used for protocol validation and selection.
/// It deliberately supports only `grpc` and `http/protobuf` — unsupported
/// protocols result in telemetry being disabled (warn + no-op), rather than
/// falling back to a default per the OTel spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraceProtocol {
    /// gRPC transport (tonic) to port 4317.
    Grpc,
    /// HTTP/Protobuf transport to port 4318.
    HttpProtobuf,
}

/// Error returned when an unsupported/invalid protocol is specified.
///
/// Includes the env var name and the offending value for diagnostic logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnsupportedProtocolError {
    /// The environment variable name that contained the invalid value.
    pub(crate) var_name: &'static str,
    /// The unsupported value that was specified.
    pub(crate) value: String,
}

impl std::fmt::Display for UnsupportedProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unsupported OTLP protocol '{}' specified in {}",
            self.value, self.var_name
        )
    }
}

impl std::error::Error for UnsupportedProtocolError {}

/// Resolves the trace export protocol from environment variables.
///
/// Reading order (signal-specific takes precedence):
/// 1. `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` (signal-specific)
/// 2. `OTEL_EXPORTER_OTLP_PROTOCOL` (generic fallback)
/// 3. Default to `HttpProtobuf` if both are absent/empty.
///
/// Empty or whitespace-only values are treated as absent (fall through to the
/// next source). This ensures accidentally-empty vars don't silently disable
/// telemetry.
///
/// Matching is EXACT and case-sensitive:
/// - `"grpc"` → `TraceProtocol::Grpc`
/// - `"http/protobuf"` → `TraceProtocol::HttpProtobuf`
///
/// Any other non-empty, non-whitespace value returns an error, enabling the
/// caller to warn and disable telemetry rather than guessing a transport.
pub(crate) fn resolve_trace_protocol() -> Result<TraceProtocol, UnsupportedProtocolError> {
    // Helper: read env var, treating empty/whitespace-only as absent.
    fn read_protocol_var(name: &'static str) -> Option<String> {
        env::var(name).ok().filter(|value| !value.trim().is_empty())
    }

    // Check signal-specific var first, then generic fallback.
    let Some((var_name, value)) = read_protocol_var(OTLP_TRACES_PROTOCOL)
        .map(|v| (OTLP_TRACES_PROTOCOL, v))
        .or_else(|| read_protocol_var(OTLP_PROTOCOL).map(|v| (OTLP_PROTOCOL, v)))
    else {
        // Both vars absent/empty → use default.
        return Ok(TraceProtocol::HttpProtobuf);
    };

    match value.as_str() {
        "grpc" => Ok(TraceProtocol::Grpc),
        "http/protobuf" => Ok(TraceProtocol::HttpProtobuf),
        _ => Err(UnsupportedProtocolError { var_name, value }),
    }
}

fn is_credential_header(name: &str) -> bool {
    name == OTLP_HEADERS || name == OTLP_TRACES_HEADERS
}

#[derive(Debug)]
struct CredentialFilteringClient<C> {
    inner: C,
    credential_headers: Vec<http::HeaderName>,
}

/// Tonic interceptor that strips credential headers from outgoing gRPC metadata.
///
/// Used when the OTLP trace endpoint is a cleartext (non-TLS) address on a non-loopback
/// host. The interceptor removes credential headers after the opentelemetry-otlp crate
/// attaches them from environment variables.
///
/// **Security invariant:** Use `http::HeaderName` (not `MetadataKey<Ascii>`) and remove
/// via `metadata_mut().as_mut()` to access the underlying `http::HeaderMap`. This
/// uniformly handles both ASCII keys and `-bin` (binary) metadata keys, avoiding the
/// security hole where `authorization-bin` would evade an ASCII-only filter. A single
/// `HeaderMap::remove` call strips all values for that key, including repeated values.
#[derive(Debug, Clone)]
pub(crate) struct CredentialFilteringInterceptor {
    credential_headers: Vec<http::HeaderName>,
}

impl tonic::service::Interceptor for CredentialFilteringInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        // Note: MetadataMap implements AsMut<http::HeaderMap>. Removing by HeaderName
        // clears both ASCII keys and `-bin` (binary) keys uniformly.
        let headers: &mut http::HeaderMap = request.metadata_mut().as_mut();
        for name in &self.credential_headers {
            headers.remove(name);
        }
        Ok(request)
    }
}

#[async_trait::async_trait]
impl<C: HttpClient> HttpClient for CredentialFilteringClient<C> {
    async fn send_bytes(
        &self,
        mut request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        for name in &self.credential_headers {
            request.headers_mut().remove(name);
        }
        self.inner.send_bytes(request).await
    }
}

/// Sets the OpenTelemetry parent context for a tracing span.
pub fn set_span_parent(span: &tracing::Span, cx: opentelemetry::Context) {
    let _ = span.set_parent(cx);
}

/// Runs a closure with an in-memory OpenTelemetry tracing subscriber.
///
/// Test support for crates that must verify spans without depending on
/// `tracing-opentelemetry` directly.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn collect_test_spans(test: impl FnOnce()) -> Vec<opentelemetry_sdk::trace::SpanData> {
    global::set_text_map_propagator(TraceContextPropagator::new());
    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("harnx-telemetry-test");
    let subscriber = Registry::default().with(tracing_opentelemetry::layer().with_tracer(tracer));

    tracing::subscriber::with_default(subscriber, test);
    provider.force_flush().expect("flush test spans");
    exporter.get_finished_spans().expect("read test spans")
}

/// Owns the tracer provider installed by [`init_telemetry`].
///
/// Call [`shutdown`](Self::shutdown) before process exit to flush pending spans.
/// The async shutdown runs `provider.shutdown()` inside `spawn_blocking` to
/// satisfy the blocking SDK contract.
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl TelemetryGuard {
    /// Shuts down the tracer provider without blocking the async runtime thread.
    pub async fn shutdown(self) {
        let Some(provider) = self.provider else {
            return;
        };

        match tokio::task::spawn_blocking(move || provider.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!("failed to shut down telemetry provider: {error}"),
            Err(error) => log::warn!("telemetry shutdown task failed: {error}"),
        }
    }

    /// Best-effort synchronous shutdown for panic and signal paths.
    pub fn shutdown_blocking(&self) {
        if let Some(provider) = &self.provider {
            if let Err(error) = provider.shutdown() {
                log::warn!("failed to shut down telemetry provider: {error}");
            }
        }
    }

    /// Returns true if the telemetry provider is active (not disabled).
    pub fn is_active(&self) -> bool {
        self.provider.is_some()
    }
}

fn strip_inherited_otel_env(command: &mut tokio::process::Command) {
    let inherited_names = env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.to_string_lossy().starts_with("OTEL_"));
    for name in inherited_names {
        command.env_remove(name);
    }
}

fn forward_otel_env_inner(command: &mut tokio::process::Command, include_credentials: bool) {
    strip_inherited_otel_env(command);
    command.env_remove(OTEL_SERVICE_NAME);

    for name in FORWARDED_OTEL_ENV {
        if !include_credentials && is_credential_header(name) {
            continue;
        }
        let Some(value) = env::var_os(name) else {
            continue;
        };
        command.env(name, value);
    }
}

/// Copies selected parent OpenTelemetry configuration into a trusted child command.
///
/// This includes OTLP credential headers. The child chooses its own service name
/// rather than inheriting the parent's.
pub fn forward_otel_env(command: &mut tokio::process::Command) {
    forward_otel_env_inner(command, true);
}

/// Copies non-secret parent OpenTelemetry configuration into an untrusted child command.
///
/// OTLP credential headers are excluded, and the child chooses its own service name.
pub fn forward_otel_env_without_credentials(command: &mut tokio::process::Command) {
    forward_otel_env_inner(command, false);
}

fn effective_traces_endpoint() -> Option<String> {
    env::var(OTLP_TRACES_ENDPOINT)
        .ok()
        .filter(|endpoint| endpoint.parse::<http::Uri>().is_ok())
        .or_else(|| env::var(OTLP_ENDPOINT).ok())
}

fn effective_credential_headers() -> Option<String> {
    env::var(OTLP_TRACES_HEADERS)
        .or_else(|_| env::var(OTLP_HEADERS))
        .ok()
}

fn should_send_headers(endpoint: &str, has_headers: bool) -> bool {
    if !has_headers {
        return true;
    }

    let Ok(uri) = endpoint.parse::<http::Uri>() else {
        return true;
    };
    if uri.scheme_str() != Some("http") {
        return true;
    }

    uri.host().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn credential_header_names(headers: &str) -> Vec<http::HeaderName> {
    headers
        .split_terminator(',')
        .filter_map(|header| {
            let (name, value) = header.trim().split_once('=')?;
            if value.trim().is_empty() {
                return None;
            }
            name.trim().parse().ok()
        })
        .collect()
}

fn effective_export_timeout() -> Duration {
    env::var(OTLP_TRACES_TIMEOUT)
        .ok()
        .and_then(|timeout| timeout.parse().ok())
        .or_else(|| {
            env::var(OTLP_TIMEOUT)
                .ok()
                .and_then(|timeout| timeout.parse().ok())
        })
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_OTLP_TIMEOUT)
}

/// Builds an HTTP/protobuf OTLP span exporter with optional credential header filtering.
fn build_http_exporter(
    filtered_headers: Option<&[http::HeaderName]>,
) -> Result<opentelemetry_otlp::SpanExporter> {
    let mut exporter_builder = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary);
    if let Some(filtered_names) = filtered_headers {
        let client = reqwest::Client::builder()
            .timeout(effective_export_timeout())
            .build()?;
        exporter_builder = exporter_builder.with_http_client(CredentialFilteringClient {
            inner: client,
            credential_headers: filtered_names.to_vec(),
        });
    }
    Ok(exporter_builder.build()?)
}

/// Builds a gRPC OTLP span exporter with optional credential header filtering.
fn build_grpc_exporter(
    filtered_headers: Option<&[http::HeaderName]>,
) -> Result<opentelemetry_otlp::SpanExporter> {
    let mut exporter_builder = SpanExporter::builder()
        .with_tonic()
        .with_protocol(Protocol::Grpc);
    if let Some(filtered_names) = filtered_headers {
        exporter_builder = exporter_builder.with_interceptor(CredentialFilteringInterceptor {
            credential_headers: filtered_names.to_vec(),
        });
    }
    Ok(exporter_builder.build()?)
}

/// Installs OTLP tracing when a standard OTLP endpoint environment variable is set.
///
/// # Failure modes
///
/// - **Unsupported protocol**: If `OTEL_EXPORTER_OTLP_PROTOCOL` or `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`
///   is set to an unrecognized value (not `grpc` or `http/protobuf`), a warning is logged and an
///   inert guard is returned with `provider: None`. Trace export is disabled for the lifetime
///   of the guard, but the process continues safely.
/// - **Exporter build failure**: If the exporter fails to build (e.g., invalid TLS configuration,
///   incompatible endpoint scheme for the transport, or tonic/reqwest construction error), the
///   error is propagated as `Err`. The caller must handle this explicitly.
///
/// The batch span processor has a default schedule_delay of 5 seconds, meaning exports
/// happen at most every 5 seconds (or immediately on shutdown). Spans are buffered
/// until the next scheduled export or explicit shutdown.
pub fn init_telemetry(service_name: &str) -> Result<TelemetryGuard> {
    if env::var_os(OTLP_ENDPOINT).is_none() && env::var_os(OTLP_TRACES_ENDPOINT).is_none() {
        return Ok(TelemetryGuard { provider: None });
    }

    // Resolve the protocol from environment variables. Unsupported values result in
    // telemetry being disabled for this run, rather than falling back to a default.
    // This is a deliberate divergence from the OTel specification (which specifies
    // fallback to default), because sending to an unsupported/unexpected transport
    // could fail or cause unwanted behavior.
    let protocol = match resolve_trace_protocol() {
        Ok(p) => p,
        Err(err) => {
            log::warn!(
                "OTLP protocol '{}' specified in {} is not supported; trace telemetry is disabled for this run (supported: grpc, http/protobuf)",
                err.value,
                err.var_name
            );
            return Ok(TelemetryGuard { provider: None });
        }
    };

    let endpoint = effective_traces_endpoint();
    let credential_headers = effective_credential_headers();

    // Determine whether credentials should be dropped from the request.
    // This applies when the endpoint is cleartext (non-TLS) and on a non-loopback host.
    // Parse header names once and emit a single warning if stripping is needed.
    let credential_headers_to_filter = if endpoint.as_deref().is_some_and(|ep| {
        let should_filter = !should_send_headers(ep, true);
        let has_creds = credential_headers
            .as_deref()
            .is_some_and(|headers| !headers.is_empty());
        should_filter && has_creds
    }) {
        log::warn!(
            "dropping OTLP credential headers because the trace endpoint uses cleartext transport \
             on a non-loopback host; configure HTTPS to send credentials"
        );
        credential_headers.as_deref().map(credential_header_names)
    } else {
        None
    };

    let filtered_ref = credential_headers_to_filter.as_deref();
    let exporter = match protocol {
        TraceProtocol::HttpProtobuf => build_http_exporter(filtered_ref)?,
        TraceProtocol::Grpc => build_grpc_exporter(filtered_ref)?,
    };

    // The batch span processor runs exporters on a Tokio async runtime. Both
    // Reqwest (HTTP) and Tonic (gRPC) async clients require an active Tokio runtime.
    // Default schedule_delay of 5s means exports happen every 5s or on shutdown.
    // Use builder() without explicit schedule_delay to rely on SDK defaults.
    let batch = BatchSpanProcessor::builder(exporter, opentelemetry_sdk::runtime::Tokio).build();
    let provider_builder = SdkTracerProvider::builder().with_span_processor(batch);
    let provider = if env::var_os(OTEL_SERVICE_NAME).is_none() {
        provider_builder
            .with_resource(
                Resource::builder()
                    .with_service_name(service_name.to_owned())
                    .build(),
            )
            .build()
    } else {
        provider_builder.build()
    };

    let tracer = provider.tracer("harnx-telemetry");
    global::set_tracer_provider(provider.clone());
    global::set_text_map_propagator(TraceContextPropagator::new());

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let subscriber = Registry::default()
        .with(filter)
        .with(tracing_opentelemetry::layer().with_tracer(tracer));
    tracing::subscriber::set_global_default(subscriber)?;

    Ok(TelemetryGuard {
        provider: Some(provider),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;

    #[derive(Clone, Debug, Default)]
    struct CapturingHttpClient {
        headers: Arc<Mutex<Option<http::HeaderMap>>>,
    }

    #[async_trait::async_trait]
    impl HttpClient for CapturingHttpClient {
        async fn send_bytes(
            &self,
            request: http::Request<Bytes>,
        ) -> Result<http::Response<Bytes>, HttpError> {
            *self.headers.lock().expect("capture request headers") =
                Some(request.headers().clone());
            Ok(http::Response::new(Bytes::new()))
        }
    }

    fn clear_endpoint_env() {
        // SAFETY: nextest runs each test in a separate process.
        unsafe {
            env::remove_var(OTLP_ENDPOINT);
            env::remove_var(OTLP_TRACES_ENDPOINT);
            // Also clear protocol vars to prevent inherited values from affecting tests
            env::remove_var(OTLP_PROTOCOL);
            env::remove_var(OTLP_TRACES_PROTOCOL);
        }
    }

    #[test]
    fn no_endpoint_returns_inert_guard_without_installing_subscriber() {
        harnx_core::require_nextest();
        clear_endpoint_env();
        let subscriber_was_set = tracing::dispatcher::has_been_set();

        let guard = init_telemetry("test-service").expect("disabled telemetry should initialize");

        assert!(guard.provider.is_none());
        assert_eq!(tracing::dispatcher::has_been_set(), subscriber_was_set);
    }

    #[tokio::test]
    async fn unreachable_endpoint_initializes_without_blocking() {
        harnx_core::require_nextest();
        clear_endpoint_env();
        // SAFETY: nextest runs each test in a separate process.
        unsafe {
            env::set_var(OTLP_ENDPOINT, "http://127.0.0.1:1");
        }
        let started = Instant::now();

        let guard = init_telemetry("test-service")
            .expect("an unreachable collector should not prevent initialization");

        assert!(guard.provider.is_some());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    const UNLISTED_OTEL_ENV: &str = "OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE";

    fn set_forwarded_otel_env() {
        // SAFETY: nextest runs each test in a separate process.
        unsafe {
            for name in FORWARDED_OTEL_ENV {
                env::set_var(name, format!("value-for-{name}"));
            }
            env::set_var(OTEL_SERVICE_NAME, "parent-service");
            env::set_var(UNLISTED_OTEL_ENV, "parent-certificate");
        }
    }

    fn assert_command_env(command: &tokio::process::Command, name: &str, expected: Option<&str>) {
        let configured = command
            .as_std()
            .get_envs()
            .find(|(configured_name, _)| configured_name.to_str() == Some(name))
            .unwrap_or_else(|| panic!("missing explicit child environment decision for {name}"));
        assert_eq!(configured.1.and_then(|value| value.to_str()), expected);
    }

    fn assert_non_secret_otel_env_forwarded(command: &tokio::process::Command) {
        for name in FORWARDED_OTEL_ENV {
            if !is_credential_header(name) {
                let expected = format!("value-for-{name}");
                assert_command_env(command, name, Some(&expected));
            }
        }
        assert_command_env(command, OTEL_SERVICE_NAME, None);
        assert_command_env(command, UNLISTED_OTEL_ENV, None);
    }

    #[test]
    fn trusted_child_gets_credentials_and_only_allowlisted_otel_env() {
        harnx_core::require_nextest();
        set_forwarded_otel_env();
        let mut command = tokio::process::Command::new("unused");

        forward_otel_env(&mut command);

        assert_non_secret_otel_env_forwarded(&command);
        assert_command_env(
            &command,
            OTLP_HEADERS,
            Some("value-for-OTEL_EXPORTER_OTLP_HEADERS"),
        );
        assert_command_env(
            &command,
            OTLP_TRACES_HEADERS,
            Some("value-for-OTEL_EXPORTER_OTLP_TRACES_HEADERS"),
        );
    }

    #[test]
    fn untrusted_child_excludes_credentials_and_unlisted_otel_env() {
        harnx_core::require_nextest();
        set_forwarded_otel_env();
        let mut command = tokio::process::Command::new("unused");

        forward_otel_env_without_credentials(&mut command);

        assert_non_secret_otel_env_forwarded(&command);
        assert_command_env(&command, OTLP_HEADERS, None);
        assert_command_env(&command, OTLP_TRACES_HEADERS, None);
    }

    #[test]
    fn unset_allowlisted_otel_env_is_not_added_to_child() {
        harnx_core::require_nextest();
        let unset_name = FORWARDED_OTEL_ENV[FORWARDED_OTEL_ENV.len() - 1];
        // SAFETY: nextest runs each test in a separate process.
        unsafe {
            env::remove_var(unset_name);
        }
        let mut command = tokio::process::Command::new("unused");

        forward_otel_env(&mut command);

        assert!(!command
            .as_std()
            .get_envs()
            .any(|(name, value)| { name.to_str() == Some(unset_name) && value.is_some() }));
    }
    #[test]
    fn cleartext_remote_endpoint_drops_credential_headers() {
        assert!(!should_send_headers("http://collector.example:4318", true));
        assert!(should_send_headers("https://collector.example:4318", true));
        assert!(should_send_headers("http://127.42.0.1:4318", true));
        assert!(should_send_headers("http://[::1]:4318", true));
        assert!(should_send_headers("http://localhost:4318", true));
        assert!(should_send_headers("http://collector.example:4318", false));
    }

    #[tokio::test]
    async fn credential_filter_removes_configured_headers_before_send() {
        let inner = CapturingHttpClient::default();
        let captured = inner.headers.clone();
        let client = CredentialFilteringClient {
            inner,
            credential_headers: vec![http::header::AUTHORIZATION],
        };
        let request = http::Request::builder()
            .header(http::header::AUTHORIZATION, "Bearer secret")
            .header(http::header::CONTENT_TYPE, "application/x-protobuf")
            .body(Bytes::new())
            .expect("build request");

        client.send_bytes(request).await.expect("send request");

        let captured = captured.lock().expect("read captured headers");
        let headers = captured.as_ref().expect("request was captured");
        assert!(!headers.contains_key(http::header::AUTHORIZATION));
        assert_eq!(
            headers.get(http::header::CONTENT_TYPE),
            Some(&http::HeaderValue::from_static("application/x-protobuf"))
        );
    }

    #[test]
    fn set_span_parent_accepts_empty_context() {
        let span = tracing::info_span!("empty_parent_test");
        set_span_parent(&span, opentelemetry::Context::new());
    }

    // === gRPC credential filter interceptor tests ===

    #[test]
    fn grpc_interceptor_strips_credential_headers_including_binary() {
        use tonic::metadata::{MetadataKey, MetadataValue};
        use tonic::service::Interceptor;

        // Build request with:
        // 1) ASCII credential key with TWO appended values (api-key)
        // 2) Binary credential key (authorization-bin)
        // 3) Non-credential ASCII key (custom-header)
        // 4) Non-credential binary key (custom-bin)
        let mut req = tonic::Request::new(());

        // ASCII credential with two values
        let key = MetadataKey::from_static("api-key");
        req.metadata_mut()
            .append(key.clone(), MetadataValue::from_static("val1"));
        req.metadata_mut()
            .append(key, MetadataValue::from_static("val2"));

        // Binary credential (-bin suffix)
        let bin_key = MetadataKey::from_static("authorization-bin");
        let bin_val = MetadataValue::from_bytes(b"secret-bytes");
        req.metadata_mut().append_bin(bin_key.clone(), bin_val);

        // Non-credential ASCII
        req.metadata_mut().insert(
            MetadataKey::from_static("custom-header"),
            MetadataValue::from_static("custom-value"),
        );

        // Non-credential binary
        let custom_bin_key = MetadataKey::from_static("custom-bin");
        let custom_bin_val = MetadataValue::from_bytes(b"custom-bytes");
        req.metadata_mut()
            .append_bin(custom_bin_key.clone(), custom_bin_val);

        // Configure interceptor with credential header names
        let mut interceptor = CredentialFilteringInterceptor {
            credential_headers: vec![
                http::HeaderName::from_static("api-key"),
                http::HeaderName::from_static("authorization-bin"),
            ],
        };

        let filtered = interceptor.call(req).expect("interceptor should succeed");

        // Assert credential keys are completely removed
        assert!(
            !filtered.metadata().contains_key("api-key"),
            "ASCII credential key 'api-key' should be removed"
        );
        assert!(
            !filtered.metadata().contains_key("authorization-bin"),
            "Binary credential key 'authorization-bin' should be removed"
        );

        // Assert non-credential keys are retained with original values
        let custom_header = filtered
            .metadata()
            .get("custom-header")
            .expect("non-credential ASCII key should be retained");
        assert_eq!(
            custom_header,
            &MetadataValue::from_static("custom-value"),
            "non-credential ASCII key should have original value"
        );

        let custom_bin = filtered
            .metadata()
            .get_bin("custom-bin")
            .expect("non-credential binary key should be retained");
        assert_eq!(
            custom_bin
                .to_bytes()
                .expect("binary value should be valid")
                .as_ref(),
            &b"custom-bytes"[..],
            "non-credential binary key should have original value"
        );
    }

    #[test]
    fn grpc_interceptor_removes_all_values_for_repeated_ascii_credential() {
        use tonic::metadata::{MetadataKey, MetadataValue};
        use tonic::service::Interceptor;

        // Verify that HeaderMap::remove clears ALL values for a repeated key
        let mut req = tonic::Request::new(());
        let key = MetadataKey::from_static("api-key");
        req.metadata_mut()
            .append(key.clone(), MetadataValue::from_static("secret1"));
        req.metadata_mut()
            .append(key.clone(), MetadataValue::from_static("secret2"));
        req.metadata_mut()
            .append(key, MetadataValue::from_static("secret3"));

        let mut interceptor = CredentialFilteringInterceptor {
            credential_headers: vec![http::HeaderName::from_static("api-key")],
        };

        let filtered = interceptor.call(req).expect("interceptor should succeed");

        assert!(
            !filtered.metadata().contains_key("api-key"),
            "all repeated values of 'api-key' should be removed"
        );
    }

    #[test]
    fn grpc_interceptor_passes_through_request_without_credentials() {
        use tonic::metadata::{MetadataKey, MetadataValue};
        use tonic::service::Interceptor;

        let mut req = tonic::Request::new(());
        req.metadata_mut().insert(
            MetadataKey::from_static("x-request-id"),
            MetadataValue::from_static("abc123"),
        );

        let mut interceptor = CredentialFilteringInterceptor {
            credential_headers: vec![http::HeaderName::from_static("authorization")],
        };

        let filtered = interceptor.call(req).expect("interceptor should succeed");

        // Non-credential headers are retained
        assert_eq!(
            filtered.metadata().get("x-request-id"),
            Some(&MetadataValue::from_static("abc123"))
        );
    }

    #[test]
    fn grpc_interceptor_handles_empty_credential_config() {
        use tonic::metadata::{MetadataKey, MetadataValue};
        use tonic::service::Interceptor;

        let mut req = tonic::Request::new(());
        req.metadata_mut().insert(
            MetadataKey::from_static("authorization"),
            MetadataValue::from_static("Bearer secret"),
        );

        let mut interceptor = CredentialFilteringInterceptor {
            credential_headers: vec![],
        };

        let filtered = interceptor.call(req).expect("interceptor should succeed");

        // With empty config, all headers pass through
        assert_eq!(
            filtered.metadata().get("authorization"),
            Some(&MetadataValue::from_static("Bearer secret"))
        );
    }

    // === Protocol resolution tests ===

    /// Helper to safely set/unset protocol env vars in tests.
    /// Uses `unsafe` because nextest runs each test in a separate process.
    struct ProtocolEnvGuard {
        vars: Vec<(&'static str, Option<String>)>,
    }

    impl ProtocolEnvGuard {
        fn new() -> Self {
            harnx_core::require_nextest();
            Self { vars: Vec::new() }
        }

        fn set(mut self, name: &'static str, value: &str) -> Self {
            // SAFETY: nextest runs each test in a separate process.
            let existing = env::var(name).ok();
            unsafe {
                env::set_var(name, value);
            }
            self.vars.push((name, existing));
            self
        }

        fn unset(mut self, name: &'static str) -> Self {
            // SAFETY: nextest runs each test in a separate process.
            let existing = env::var(name).ok();
            unsafe {
                env::remove_var(name);
            }
            self.vars.push((name, existing));
            self
        }
    }

    impl Drop for ProtocolEnvGuard {
        fn drop(&mut self) {
            // SAFETY: nextest runs each test in a separate process.
            // Restore in reverse order (LIFO) to handle nested mutations correctly.
            unsafe {
                for (name, value) in self.vars.iter().rev() {
                    match value {
                        Some(v) => env::set_var(name, v),
                        None => env::remove_var(name),
                    }
                }
            }
        }
    }

    fn clear_protocol_env() -> ProtocolEnvGuard {
        ProtocolEnvGuard::new()
            .unset(OTLP_PROTOCOL)
            .unset(OTLP_TRACES_PROTOCOL)
    }

    #[test]
    fn protocol_unset_both_returns_http_protobuf_default() {
        let _guard = clear_protocol_env();
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::HttpProtobuf));
    }

    #[test]
    fn protocol_generic_grpc_returns_grpc() {
        let _guard = clear_protocol_env().set(OTLP_PROTOCOL, "grpc");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::Grpc));
    }

    #[test]
    fn protocol_generic_http_protobuf_returns_http_protobuf() {
        let _guard = clear_protocol_env().set(OTLP_PROTOCOL, "http/protobuf");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::HttpProtobuf));
    }

    #[test]
    fn protocol_traces_overrides_generic_to_grpc() {
        let _guard = clear_protocol_env()
            .set(OTLP_PROTOCOL, "http/protobuf")
            .set(OTLP_TRACES_PROTOCOL, "grpc");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::Grpc));
    }

    #[test]
    fn protocol_traces_overrides_generic_to_http_protobuf() {
        let _guard = clear_protocol_env()
            .set(OTLP_PROTOCOL, "grpc")
            .set(OTLP_TRACES_PROTOCOL, "http/protobuf");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::HttpProtobuf));
    }

    #[test]
    fn protocol_unsupported_http_json_returns_error() {
        let _guard = clear_protocol_env().set(OTLP_PROTOCOL, "http/json");
        let result = resolve_trace_protocol();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.var_name, OTLP_PROTOCOL);
        assert_eq!(err.value, "http/json");
    }

    #[test]
    fn protocol_unsupported_grpc_web_returns_error() {
        let _guard = clear_protocol_env().set(OTLP_PROTOCOL, "grpc-web");
        let result = resolve_trace_protocol();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.var_name, OTLP_PROTOCOL);
        assert_eq!(err.value, "grpc-web");
    }

    #[test]
    fn protocol_unsupported_uppercase_grpc_returns_error() {
        let _guard = clear_protocol_env().set(OTLP_PROTOCOL, "GRPC");
        let result = resolve_trace_protocol();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.var_name, OTLP_PROTOCOL);
        assert_eq!(err.value, "GRPC");
    }

    #[test]
    fn protocol_unsupported_typo_returns_error() {
        let _guard = clear_protocol_env().set(OTLP_PROTOCOL, "grp");
        let result = resolve_trace_protocol();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.var_name, OTLP_PROTOCOL);
        assert_eq!(err.value, "grp");
    }

    #[test]
    fn protocol_empty_generic_falls_through_to_default() {
        // Empty generic var should be treated as absent -> default HttpProtobuf
        let _guard = clear_protocol_env().set(OTLP_PROTOCOL, "");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::HttpProtobuf));
    }

    #[test]
    fn protocol_empty_traces_with_grpc_generic_uses_grpc() {
        // Empty signal-specific var falls through to generic
        let _guard = clear_protocol_env()
            .set(OTLP_PROTOCOL, "grpc")
            .set(OTLP_TRACES_PROTOCOL, "");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::Grpc));
    }

    #[test]
    fn protocol_whitespace_only_traces_falls_through_to_default() {
        // Whitespace-only signal-specific var treated as absent
        let _guard = clear_protocol_env().set(OTLP_TRACES_PROTOCOL, "   ");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::HttpProtobuf));
    }

    #[test]
    fn protocol_whitespace_only_generic_falls_through_to_default() {
        // Whitespace-only generic var treated as absent
        let _guard = clear_protocol_env().set(OTLP_PROTOCOL, "   ");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::HttpProtobuf));
    }

    #[test]
    fn protocol_empty_traces_empty_generic_returns_default() {
        // Both empty -> default
        let _guard = clear_protocol_env()
            .set(OTLP_PROTOCOL, "")
            .set(OTLP_TRACES_PROTOCOL, "");
        assert_eq!(resolve_trace_protocol(), Ok(TraceProtocol::HttpProtobuf));
    }

    // === init_telemetry tests ===

    #[tokio::test]
    async fn unsupported_protocol_disables_telemetry_cleanly() {
        harnx_core::require_nextest();
        // Setting an unsupported protocol should not crash; it should return a no-op guard.
        clear_endpoint_env();
        // SAFETY: nextest runs each test in a separate process.
        unsafe {
            env::set_var(OTLP_ENDPOINT, "http://localhost:4318");
            env::set_var(OTLP_PROTOCOL, "http/json");
        }

        let guard = init_telemetry("test-service")
            .expect("unsupported protocol should not error, just disable telemetry");

        // Guard should have no provider (telemetry disabled)
        assert!(
            guard.provider.is_none(),
            "unsupported protocol should yield a no-op TelemetryGuard"
        );
    }

    #[tokio::test]
    async fn grpc_with_cleartext_remote_endpoint_returns_active_guard() {
        harnx_core::require_nextest();
        // Verify that init_telemetry succeeds for gRPC with cleartext non-loopback endpoint
        // and returns an active guard (provider.is_some()).
        clear_endpoint_env();
        // SAFETY: nextest runs each test in a separate process.
        unsafe {
            env::set_var(OTLP_ENDPOINT, "http://collector.example:4317");
            env::set_var(OTLP_PROTOCOL, "grpc");
            env::set_var(OTLP_HEADERS, "api-key=secret,authorization=token");
        }

        let guard = init_telemetry("test-service")
            .expect("gRPC with cleartext remote endpoint should initialize successfully");

        assert!(
            guard.provider.is_some(),
            "gRPC with cleartext remote endpoint should yield an active TelemetryGuard"
        );
    }

    #[test]
    fn invalid_traces_protocol_returns_unsupported_protocol_error_with_correct_var_name() {
        harnx_core::require_nextest();
        let _guard = clear_protocol_env().set(OTLP_TRACES_PROTOCOL, "invalid-protocol");
        let result = resolve_trace_protocol();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.var_name, OTLP_TRACES_PROTOCOL);
        assert_eq!(err.value, "invalid-protocol");
    }
}
