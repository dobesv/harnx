//! OpenTelemetry tracing initialization for harnx services.
//!
//! This crate provides env-gated OTLP HTTP tracing setup and W3C trace-context
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
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

const DEFAULT_FILTER: &str = "off,harnx=info";
const OTLP_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const OTLP_TRACES_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT";
const OTLP_HEADERS: &str = "OTEL_EXPORTER_OTLP_HEADERS";
const OTLP_TRACES_HEADERS: &str = "OTEL_EXPORTER_OTLP_TRACES_HEADERS";
const OTLP_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_TIMEOUT";
const OTLP_TRACES_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT";
const OTEL_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
const DEFAULT_OTLP_TIMEOUT: Duration = Duration::from_secs(10);
const FORWARDED_OTEL_ENV: [&str; 8] = [
    OTLP_ENDPOINT,
    OTLP_TRACES_ENDPOINT,
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    OTLP_HEADERS,
    OTLP_TRACES_HEADERS,
    "OTEL_RESOURCE_ATTRIBUTES",
    "OTEL_TRACES_SAMPLER",
    "OTEL_TRACES_SAMPLER_ARG",
];

fn is_credential_header(name: &str) -> bool {
    name == OTLP_HEADERS || name == OTLP_TRACES_HEADERS
}

#[derive(Debug)]
struct CredentialFilteringClient<C> {
    inner: C,
    credential_headers: Vec<http::HeaderName>,
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

/// Installs OTLP tracing when a standard OTLP endpoint environment variable is set.
pub fn init_telemetry(service_name: &str) -> Result<TelemetryGuard> {
    if env::var_os(OTLP_ENDPOINT).is_none() && env::var_os(OTLP_TRACES_ENDPOINT).is_none() {
        return Ok(TelemetryGuard { provider: None });
    }

    let endpoint = effective_traces_endpoint();
    let credential_headers = effective_credential_headers();
    let mut exporter_builder = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary);
    if let Some(headers) = credential_headers.as_deref().filter(|_| {
        endpoint
            .as_deref()
            .is_some_and(|endpoint| !should_send_headers(endpoint, true))
    }) {
        log::warn!(
            "dropping OTLP credential headers because the trace endpoint uses cleartext HTTP on a non-loopback host; configure HTTPS to send credentials"
        );
        let client = reqwest::Client::builder()
            .timeout(effective_export_timeout())
            .build()?;
        exporter_builder = exporter_builder.with_http_client(CredentialFilteringClient {
            inner: client,
            credential_headers: credential_header_names(headers),
        });
    }
    let exporter = exporter_builder.build()?;

    // The default batch processor polls exporters on a plain OS thread. Reqwest's
    // async client needs the Tokio runtime that is active during service startup.
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
        }
    }

    #[test]
    fn no_endpoint_returns_inert_guard_without_installing_subscriber() {
        clear_endpoint_env();
        let subscriber_was_set = tracing::dispatcher::has_been_set();

        let guard = init_telemetry("test-service").expect("disabled telemetry should initialize");

        assert!(guard.provider.is_none());
        assert_eq!(tracing::dispatcher::has_been_set(), subscriber_was_set);
    }

    #[tokio::test]
    async fn unreachable_endpoint_initializes_without_blocking() {
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
        set_forwarded_otel_env();
        let mut command = tokio::process::Command::new("unused");

        forward_otel_env_without_credentials(&mut command);

        assert_non_secret_otel_env_forwarded(&command);
        assert_command_env(&command, OTLP_HEADERS, None);
        assert_command_env(&command, OTLP_TRACES_HEADERS, None);
    }

    #[test]
    fn unset_allowlisted_otel_env_is_not_added_to_child() {
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
}
