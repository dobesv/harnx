//! W3C trace-context propagation across harnx transport boundaries.

use http::HeaderMap;
use opentelemetry::{global, Context};
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Injects the current tracing span's OpenTelemetry context into HTTP headers.
pub fn inject_current_into_http(headers: &mut HeaderMap) {
    let context = tracing::Span::current().context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut HeaderInjector(headers));
    });
}

/// Extracts an OpenTelemetry context from HTTP headers.
pub fn extract_context_from_http(headers: &HeaderMap) -> Context {
    global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)))
}

#[cfg(feature = "nats")]
mod nats {
    use async_nats::HeaderMap;
    use opentelemetry::propagation::{Extractor, Injector};
    use opentelemetry::{global, Context};
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    struct NatsHeaderInjector<'a>(&'a mut HeaderMap);

    impl Injector for NatsHeaderInjector<'_> {
        fn set(&mut self, key: &str, value: String) {
            self.0.insert(key, value);
        }
    }

    struct NatsHeaderExtractor<'a>(&'a HeaderMap);

    impl Extractor for NatsHeaderExtractor<'_> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).map(|value| value.as_str())
        }

        fn keys(&self) -> Vec<&str> {
            self.0
                .iter()
                .map(|(name, _)| -> &str { name.as_ref() })
                .collect()
        }
    }

    /// Injects the current tracing span's OpenTelemetry context into NATS headers.
    pub fn inject_current_into_nats(headers: &mut HeaderMap) {
        let context = tracing::Span::current().context();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut NatsHeaderInjector(headers));
        });
    }

    /// Extracts an OpenTelemetry context from NATS headers.
    pub fn extract_context_from_nats(headers: &HeaderMap) -> Context {
        global::get_text_map_propagator(|propagator| {
            propagator.extract(&NatsHeaderExtractor(headers))
        })
    }

    #[cfg(test)]
    pub(super) fn contains_key(headers: &HeaderMap, expected: &str) -> bool {
        NatsHeaderExtractor(headers).keys().contains(&expected)
    }
}

#[cfg(feature = "nats")]
pub use nats::{extract_context_from_nats, inject_current_into_nats};

#[cfg(feature = "mcp")]
mod mcp {
    use opentelemetry::propagation::{Extractor, Injector};
    use opentelemetry::{global, Context};
    use rmcp::model::{CallToolRequestParams, RequestMetaObject, RequestParamsMeta};
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    const TRACEPARENT: &str = "traceparent";
    const TRACESTATE: &str = "tracestate";

    struct McpParamsInjector<'a>(&'a mut CallToolRequestParams);

    impl Injector for McpParamsInjector<'_> {
        fn set(&mut self, key: &str, value: String) {
            match key {
                TRACEPARENT => self.0.set_traceparent(&value),
                TRACESTATE => self.0.set_tracestate(&value),
                _ => {}
            }
        }
    }

    struct McpFieldsExtractor<'a> {
        traceparent: Option<&'a str>,
        tracestate: Option<&'a str>,
    }

    impl Extractor for McpFieldsExtractor<'_> {
        fn get(&self, key: &str) -> Option<&str> {
            match key {
                TRACEPARENT => self.traceparent,
                TRACESTATE => self.tracestate,
                _ => None,
            }
        }

        fn keys(&self) -> Vec<&str> {
            let mut keys = Vec::with_capacity(2);
            if self.traceparent.is_some() {
                keys.push(TRACEPARENT);
            }
            if self.tracestate.is_some() {
                keys.push(TRACESTATE);
            }
            keys
        }
    }

    fn extract(traceparent: Option<&str>, tracestate: Option<&str>) -> Context {
        global::get_text_map_propagator(|propagator| {
            propagator.extract(&McpFieldsExtractor {
                traceparent,
                tracestate,
            })
        })
    }

    /// Injects the current tracing span's OpenTelemetry context into MCP call params.
    pub fn inject_current_into_mcp(params: &mut CallToolRequestParams) {
        let context = tracing::Span::current().context();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut McpParamsInjector(params));
        });
    }

    /// Extracts an OpenTelemetry context from MCP call params `_meta`.
    pub fn extract_context_from_mcp(params: &CallToolRequestParams) -> Context {
        extract(params.traceparent(), params.tracestate())
    }

    /// Extracts context from rmcp's request context metadata.
    ///
    /// rmcp moves `_meta` out of request params before invoking a server handler,
    /// so live server handlers must read `RequestContext::meta` instead.
    pub fn extract_context_from_mcp_meta(meta: &RequestMetaObject) -> Context {
        extract(meta.get_traceparent(), meta.get_tracestate())
    }
}

#[cfg(feature = "mcp")]
pub use mcp::{extract_context_from_mcp, extract_context_from_mcp_meta, inject_current_into_mcp};

#[cfg(test)]
mod tests {
    use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    fn with_trace_context(test: impl FnOnce(opentelemetry::trace::TraceId)) {
        global::set_text_map_propagator(TraceContextPropagator::new());
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("propagation-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("propagation_test");
            let _entered = span.enter();
            let trace_id = tracing::Span::current()
                .context()
                .span()
                .span_context()
                .trace_id();
            assert!(trace_id != opentelemetry::trace::TraceId::INVALID);
            test(trace_id);
        });
    }

    fn trace_id(context: &Context) -> opentelemetry::trace::TraceId {
        context.span().span_context().trace_id()
    }

    #[test]
    fn http_context_round_trip_and_empty_extract() {
        let empty = HeaderMap::new();
        assert_eq!(
            trace_id(&extract_context_from_http(&empty)),
            opentelemetry::trace::TraceId::INVALID
        );

        with_trace_context(|expected| {
            let mut headers = HeaderMap::new();
            inject_current_into_http(&mut headers);

            assert!(headers.contains_key("traceparent"));
            assert_eq!(trace_id(&extract_context_from_http(&headers)), expected);
        });
    }

    #[cfg(feature = "nats")]
    #[test]
    fn nats_context_round_trip_and_empty_extract() {
        let empty = async_nats::HeaderMap::new();
        assert_eq!(
            trace_id(&extract_context_from_nats(&empty)),
            opentelemetry::trace::TraceId::INVALID
        );

        with_trace_context(|expected| {
            let mut headers = async_nats::HeaderMap::new();
            inject_current_into_nats(&mut headers);

            assert!(headers.get("traceparent").is_some());
            assert!(nats::contains_key(&headers, "traceparent"));
            assert_eq!(trace_id(&extract_context_from_nats(&headers)), expected);
        });
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn mcp_context_round_trip_and_empty_extract() {
        use rmcp::model::{CallToolRequestParams, RequestParamsMeta};

        let empty = CallToolRequestParams::new("test");
        assert_eq!(
            trace_id(&extract_context_from_mcp(&empty)),
            opentelemetry::trace::TraceId::INVALID
        );

        with_trace_context(|expected| {
            let mut params = CallToolRequestParams::new("test");
            inject_current_into_mcp(&mut params);

            assert!(params.traceparent().is_some());
            assert_eq!(trace_id(&extract_context_from_mcp(&params)), expected);
            assert_eq!(
                trace_id(&extract_context_from_mcp_meta(
                    params.meta.as_ref().expect("request _meta")
                )),
                expected
            );
        });
    }
}
