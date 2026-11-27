//! Upstream MCP calls, MRTR mapping and cooperative cancellation.

use harnx_toolset::{ToolInvocation, ToolInvokeError};
use harnx_toolset_server::mrtr;
use serde_json::Value;

fn call_params(
    invocation: &ToolInvocation,
) -> Result<rmcp::model::CallToolRequestParams, ToolInvokeError> {
    let arguments = match &invocation.args {
        Value::Object(arguments) => Some(arguments.clone()),
        Value::Null => None,
        _ => {
            return Err(ToolInvokeError::Recoverable(
                "MCP tool arguments must be a JSON object or null".into(),
            ))
        }
    };
    let mut params = rmcp::model::CallToolRequestParams::new(invocation.tool.clone());
    if let Some(arguments) = arguments {
        params = params.with_arguments(arguments);
    }
    if !invocation.context.input_responses.is_empty() {
        params.input_responses = Some(harnx_toolset_server::mrtr::responses_to_mrtr(
            &invocation.context.input_responses,
        ));
    }
    if !invocation.context.request_state.is_null() {
        params.request_state = mrtr::request_state_from_value(&invocation.context.request_state)
            .map_err(ToolInvokeError::Recoverable)?;
    }
    // The structured-result request rides in `_meta` beside the arguments, the
    // bridge's pattern: present only when the caller asked, so an upstream
    // that keys on it serves model-style results by default.
    if invocation.context.wants_structured_result() {
        params.meta.get_or_insert_default().insert(
            harnx_toolset::CAPABILITY_STRUCTURED_RESULT.to_owned(),
            Value::Bool(true),
        );
    }
    harnx_telemetry::propagate::inject_current_into_mcp(&mut params);
    Ok(params)
}

/// Dispatch one upstream `tools/call`, mirroring the bridge's MRTR handling:
/// responses and state ride beside the arguments, an upstream
/// `input_required` becomes a harnx challenge, and cancellation is
/// cooperative. Upstream-sourced text (JSON-RPC errors, tool result
/// content) is redacted against the in-use credential first.
pub(super) async fn call_upstream(
    invocation: &ToolInvocation,
    client: &crate::native::pool::PoolClient,
) -> Result<Value, ToolInvokeError> {
    let peer = &client.peer;
    let params = call_params(invocation)?;
    if invocation.cancel.is_cancelled() {
        return Err(ToolInvokeError::Recoverable("call cancelled".into()));
    }
    let mut request = peer
        .send_cancellable_request(
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(params)),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await
        .map_err(|error| upstream_error(error, client))?;
    let result = tokio::select! {
        biased;
        _ = invocation.cancel.cancelled() => {
            harnx_toolset::cleanup::unconfirmed("MCP cancellation notified best-effort; remote shutdown not confirmed");
            request.rx.close();
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                request.cancel(Some("invocation cancelled".into())),
            )
            .await;
            return Err(ToolInvokeError::Recoverable("invocation cancelled".into()));
        }
        result = &mut request.rx => result
            .map_err(|_| ToolInvokeError::Fatal("remote MCP server exited during call".into()))
            .and_then(|result| result.map_err(|error| upstream_error(error, client)))?,
    };
    match result {
        rmcp::model::ServerResult::CallToolResult(result) => serialize_result(result, client),
        rmcp::model::ServerResult::InputRequiredResult(input_required) => {
            harnx_toolset_server::mrtr::challenge_from_mrtr(&input_required)
                .map(|challenge| Err(ToolInvokeError::InputRequired(Box::new(challenge))))
                .unwrap_or_else(|error| Err(ToolInvokeError::Recoverable(error)))
        }
        _ => Err(ToolInvokeError::Recoverable(
            "unexpected MCP response for tools/call".into(),
        )),
    }
}

/// Serialize one upstream tool result, redacting the in-use credential from
/// every string on the way out.
fn serialize_result(
    result: rmcp::model::CallToolResult,
    client: &crate::native::pool::PoolClient,
) -> Result<Value, ToolInvokeError> {
    let mut value = serde_json::to_value(result)
        .map_err(|_| ToolInvokeError::Recoverable("MCP tool result is not serializable".into()))?;
    redact_value(&mut value, client);
    Ok(value)
}

/// Walk a serialized tool result and redact the in-use credential from every
/// string: a provider can echo the submitted token in result content.
// qual:allow(iosp) reason: "recursive serde_json walk is match + recurse by nature; each arm is one line"
fn redact_value(value: &mut Value, client: &crate::native::pool::PoolClient) {
    match value {
        Value::String(text) => {
            if let std::borrow::Cow::Owned(redacted) = client.redact(text) {
                *text = redacted;
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, client);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                redact_value(item, client);
            }
        }
        _ => {}
    }
}

/// Map upstream service errors to recoverable tool errors. The text is
/// provider-sourced, so the in-use credential is redacted before it can
/// reach a tool result or the journal.
fn upstream_error(
    error: rmcp::service::ServiceError,
    client: &crate::native::pool::PoolClient,
) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!(
        "upstream call failed: {}",
        client.redact(&error.to_string())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_toolset::{ToolInvocation, ToolInvocationContext};
    use opentelemetry::global;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use rmcp::model::RequestParamsMeta as _;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;
    use tracing_subscriber::layer::SubscriberExt as _;

    fn invocation(capabilities: &[&str]) -> ToolInvocation {
        ToolInvocation {
            tool: "search".into(),
            args: json!({ "q": "needle" }),
            context: ToolInvocationContext {
                capabilities: capabilities.iter().map(|name| name.to_string()).collect(),
                ..Default::default()
            },
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn structured_result_capability_is_forwarded_when_requested() {
        harnx_core::require_nextest();
        let key = harnx_toolset::CAPABILITY_STRUCTURED_RESULT;
        let params = call_params(&invocation(&[key])).expect("params");
        assert_eq!(params.name, "search");
        let meta = params.meta.as_ref().expect("request _meta");
        assert_eq!(meta.get(key), Some(&Value::Bool(true)));
    }

    #[test]
    fn structured_result_capability_is_omitted_when_not_requested() {
        harnx_core::require_nextest();
        let key = harnx_toolset::CAPABILITY_STRUCTURED_RESULT;
        let params = call_params(&invocation(&[])).expect("params");
        let present = params
            .meta
            .as_ref()
            .is_some_and(|meta| meta.get(key).is_some());
        assert!(!present, "capability must not be sent unasked: {params:?}");
    }

    #[test]
    fn structured_result_trace_context_stays_beside_the_capability() {
        harnx_core::require_nextest();
        global::set_text_map_propagator(TraceContextPropagator::new());
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("mcp-remote-structured-result-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("mcp_remote_structured_result_test");
            let _entered = span.enter();
            let key = harnx_toolset::CAPABILITY_STRUCTURED_RESULT;
            let params = call_params(&invocation(&[key])).expect("params");
            let meta = params.meta.as_ref().expect("request _meta");
            assert_eq!(meta.get(key), Some(&Value::Bool(true)));
            assert!(
                params.traceparent().is_some(),
                "traceparent stays beside it"
            );
        });
    }
}
