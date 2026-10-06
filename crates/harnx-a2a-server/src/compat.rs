//! Input compatibility only. Responses always use the upstream A2A 1.0 codec.
//!
//! Missing versions and explicit 0.3 use the 1.0 interface, with method aliases,
//! legacy blocking and enum spellings accepted. This is not a 0.3 wire binding.

use a2a_lf::{
    jsonrpc::{JsonRpcId, JsonRpcRequest, JsonRpcResponse},
    A2AError,
};
use a2a_server_lf::jsonrpc::MAX_REQUEST_BODY_BYTES;
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{header::CONTENT_LENGTH, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;

const METHOD_ALIASES: &[(&str, &str)] = &[
    ("message/send", "SendMessage"),
    ("message/stream", "SendStreamingMessage"),
    ("tasks/get", "GetTask"),
    ("tasks/cancel", "CancelTask"),
    ("tasks/resubscribe", "SubscribeToTask"),
    (
        "tasks/pushNotificationConfig/set",
        "CreateTaskPushNotificationConfig",
    ),
    (
        "tasks/pushNotificationConfig/get",
        "GetTaskPushNotificationConfig",
    ),
    (
        "tasks/pushNotificationConfig/list",
        "ListTaskPushNotificationConfigs",
    ),
    (
        "tasks/pushNotificationConfig/delete",
        "DeleteTaskPushNotificationConfig",
    ),
];

const ROLE_VARIANTS: &[&str] = &["UNSPECIFIED", "USER", "AGENT"];

const TASK_STATE_VARIANTS: &[&str] = &[
    "UNSPECIFIED",
    "SUBMITTED",
    "WORKING",
    "COMPLETED",
    "FAILED",
    "CANCELED",
    "INPUT_REQUIRED",
    "REJECTED",
    "AUTH_REQUIRED",
];

pub(crate) async fn normalize(mut request: Request, next: Next) -> Response {
    let body = match to_bytes(std::mem::take(request.body_mut()), MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let mut value = serde_json::from_slice::<Value>(&body).ok();
    // Let upstream produce parse/invalid-request errors before interpreting params.
    let envelope = serde_json::from_slice::<JsonRpcRequest>(&body).ok();
    let valid_envelope = envelope.as_ref().filter(|rpc| rpc.jsonrpc == "2.0");

    if let Err(response) = check_version(request.headers_mut(), valid_envelope) {
        return *response;
    }

    if let (Some(rpc), Some(value)) = (valid_envelope, value.as_mut()) {
        match process_payload(rpc, value) {
            Ok(bytes) => {
                *request.body_mut() = Body::from(bytes);
                request.headers_mut().remove(CONTENT_LENGTH);
            }
            Err(response) => return *response,
        }
    } else {
        *request.body_mut() = Body::from(body);
    }
    next.run(request).await
}

fn check_version(
    headers: &mut HeaderMap,
    valid_envelope: Option<&JsonRpcRequest>,
) -> Result<(), Box<Response>> {
    let version = headers.get("a2a-version");
    let supported = match version {
        None => {
            tracing::debug!("A2A-Version missing; treating request as A2A 1.0");
            true
        }
        Some(version) => version.to_str().is_ok_and(supported_version),
    };
    if supported {
        // Upstream defaults missing versions to unsupported 0.3. Also narrow
        // its major-only check to the interface versions we actually support.
        headers.insert("a2a-version", HeaderValue::from_static("1.0"));
        return Ok(());
    }
    if let Some(rpc) = valid_envelope {
        let raw_version = version
            .and_then(|v| v.to_str().ok())
            .unwrap_or("invalid header");
        return Err(Box::new(error(
            rpc.id.clone(),
            A2AError::version_not_supported(raw_version),
        )));
    }
    Ok(())
}

fn process_payload(rpc: &JsonRpcRequest, value: &mut Value) -> Result<Vec<u8>, Box<Response>> {
    if value
        .get("params")
        .is_some_and(|params| !params.is_object() && !params.is_array())
    {
        return Err(Box::new(error(
            rpc.id.clone(),
            A2AError::invalid_request("params must be an object or array"),
        )));
    }
    if let Err(err) = normalize_json(value) {
        return Err(Box::new(error(rpc.id.clone(), err)));
    }
    let bytes = serde_json::to_vec(value).expect("JSON value serializes");
    if bytes.len() > MAX_REQUEST_BODY_BYTES {
        return Err(Box::new(StatusCode::PAYLOAD_TOO_LARGE.into_response()));
    }
    Ok(bytes)
}

fn supported_version(version: &str) -> bool {
    let version = version.trim();
    matches!(version, "1.0" | "0.3")
        || version.strip_prefix("1.0.").is_some_and(|patch| {
            !patch.is_empty() && patch.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn error(id: JsonRpcId, error: A2AError) -> Response {
    // Match upstream JSON-RPC envelopes, including typed ErrorInfo details.
    Json(JsonRpcResponse::error(id, error.to_jsonrpc_error())).into_response()
}

fn canonical_method(method: &str) -> &str {
    METHOD_ALIASES
        .iter()
        .find_map(|&(alias, canonical)| (alias == method).then_some(canonical))
        .unwrap_or(method)
}

fn normalize_blocking(configuration: &mut serde_json::Map<String, Value>) -> Result<(), A2AError> {
    let Some(blocking) = configuration.remove("blocking") else {
        return Ok(());
    };
    let blocking = blocking
        .as_bool()
        .ok_or_else(|| A2AError::invalid_params("blocking must be a boolean"))?;
    for immediate in ["returnImmediately", "return_immediately"]
        .iter()
        .filter_map(|key| configuration.get(*key))
    {
        let immediate = immediate
            .as_bool()
            .ok_or_else(|| A2AError::invalid_params("returnImmediately must be a boolean"))?;
        if immediate == blocking {
            return Err(A2AError::invalid_params(
                "blocking conflicts with returnImmediately",
            ));
        }
    }
    configuration.remove("return_immediately");
    configuration.insert("returnImmediately".into(), Value::Bool(!blocking));
    Ok(())
}

fn normalize_send_params(params: &mut Value) -> Result<(), A2AError> {
    if let Some(configuration) = params
        .get_mut("configuration")
        .and_then(Value::as_object_mut)
    {
        normalize_blocking(configuration)?;
    }
    if let Some(role) = params
        .get_mut("message")
        .and_then(|message| message.get_mut("role"))
    {
        normalize_enum(role, "ROLE_", ROLE_VARIANTS);
    }
    Ok(())
}

fn normalize_list_tasks_params(params: &mut Value) {
    if let Some(state) = params.get_mut("status") {
        normalize_enum(state, "TASK_STATE_", TASK_STATE_VARIANTS);
    }
}

fn normalize_json(request: &mut Value) -> Result<(), A2AError> {
    let method = request["method"].as_str().unwrap_or_default();
    let method = canonical_method(method).to_owned();
    request["method"] = Value::String(method.clone());
    let Some(params) = request.get_mut("params") else {
        return Ok(());
    };
    match method.as_str() {
        "SendMessage" | "SendStreamingMessage" => normalize_send_params(params)?,
        "ListTasks" => normalize_list_tasks_params(params),
        _ => {}
    }
    Ok(())
}

fn normalize_enum(value: &mut Value, prefix: &str, variants: &[&str]) {
    let Some(input) = value.as_str() else {
        return;
    };
    let upper = input.replace('-', "_").to_ascii_uppercase();
    let name = upper.strip_prefix(prefix).unwrap_or(&upper);
    if variants.contains(&name) {
        *value = Value::String(format!("{prefix}{name}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn app() -> Router {
        harnx_core::require_nextest();
        a2a_server_lf::jsonrpc::jsonrpc_router(Arc::new(crate::test_support::RoutingHandler))
            .route_layer(axum::middleware::from_fn(normalize))
    }
    async fn rpc(body: &str, version: Option<&str>) -> Value {
        let mut request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json");
        if let Some(version) = version {
            request = request.header("a2a-version", version);
        }
        let response = app()
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }

    #[tokio::test]
    async fn compat_aliases_dispatch_and_preserve_canonical_method() {
        for (alias, canonical, params, code) in [
            (
                "message/send",
                "SendMessage",
                json!({"message":{"role":"user","messageId":"m","parts":[{"text":"hello"}]}}),
                -32004,
            ),
            (
                "message/stream",
                "SendStreamingMessage",
                json!({"message":{"role":"user","messageId":"m","parts":[{"text":"hello"}]}}),
                -32004,
            ),
            ("tasks/get", "GetTask", json!({"id":"t"}), -32004),
            ("tasks/cancel", "CancelTask", json!({"id":"t"}), -32004),
            (
                "tasks/resubscribe",
                "SubscribeToTask",
                json!({"id":"t"}),
                -32004,
            ),
            (
                "tasks/pushNotificationConfig/set",
                "CreateTaskPushNotificationConfig",
                json!({"taskId":"t","pushNotificationConfig":{"id":"p","url":"https://example.com/push"}}),
                -32003,
            ),
            (
                "tasks/pushNotificationConfig/get",
                "GetTaskPushNotificationConfig",
                json!({"taskId":"t","id":"p"}),
                -32003,
            ),
            (
                "tasks/pushNotificationConfig/list",
                "ListTaskPushNotificationConfigs",
                json!({"taskId":"t"}),
                -32003,
            ),
            (
                "tasks/pushNotificationConfig/delete",
                "DeleteTaskPushNotificationConfig",
                json!({"taskId":"t","id":"p"}),
                -32003,
            ),
        ] {
            let mut request = json!({"jsonrpc":"2.0","id":"alias","method":alias,"params":params});
            let response = rpc(&request.to_string(), Some("1.0")).await;
            assert_eq!(response["error"]["code"], code, "{alias}: {response}");
            assert_eq!(response["id"], "alias");
            normalize_json(&mut request).unwrap();
            assert_eq!(request["method"], canonical);
            let response = rpc(&request.to_string(), Some("1.0")).await;
            assert_eq!(response["error"]["code"], code);
        }
    }

    #[tokio::test]
    async fn compat_standard_error_codes() {
        for (body, code, id) in [
            ("{", -32700, Value::Null),
            ("", -32700, Value::Null),
            ("[]", -32600, Value::Null),
            (
                r#"{"jsonrpc":"2.0","id":7,"method":"GetTask","params":null}"#,
                -32600,
                json!(7),
            ),
            (
                r#"{"jsonrpc":"2.0","id":7,"method":"SendMessage","params":{"message":{"role":"bogus"}}}"#,
                -32602,
                json!(7),
            ),
            (r#"{"jsonrpc":"2.0","id":7,"params":{}}"#, -32600, json!(7)),
            (
                r#"{"jsonrpc":"1.0","id":7,"method":"GetTask"}"#,
                -32600,
                json!(7),
            ),
            (
                r#"{"jsonrpc":"2.0","id":true,"method":"GetTask"}"#,
                -32600,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","id":7,"method":"GetTask","params":true}"#,
                -32600,
                json!(7),
            ),
            (
                r#"{"jsonrpc":"2.0","id":7,"method":"unknown","params":{}}"#,
                -32601,
                json!(7),
            ),
            (
                r#"{"jsonrpc":"2.0","id":7,"method":"message/send","params":{"message":"bad"}}"#,
                -32602,
                json!(7),
            ),
            (
                r#"{"jsonrpc":"2.0","id":7,"method":"message/stream","params":{"message":"bad"}}"#,
                -32602,
                json!(7),
            ),
            (
                r#"{"jsonrpc":"2.0","id":7,"method":"tasks/get","params":{"id":17}}"#,
                -32602,
                json!(7),
            ),
            (
                r#"{"jsonrpc":"2.0","id":7,"method":"ListTasks","params":{"status":"bogus"}}"#,
                -32602,
                json!(7),
            ),
        ] {
            let response = rpc(body, None).await;
            assert_eq!(response["error"]["code"], code, "{body}: {response}");
            assert_eq!(response["id"], id, "{response}");
            assert_eq!(response["jsonrpc"], "2.0");
            assert!(response.get("result").is_none());
        }
    }

    #[tokio::test]
    async fn compat_version_policy() {
        for (version, code) in [
            (None, -32004),
            (Some("1.0"), -32004),
            (Some("1.0.12"), -32004),
            (Some("0.3"), -32004),
            (Some("2.0"), -32009),
            (Some("0.2"), -32009),
            (Some("1.1"), -32009),
            (Some("1.0.bad"), -32009),
            (Some("1.0."), -32009),
            (Some("1"), -32009),
            (Some(""), -32009),
        ] {
            let response = rpc(
                r#"{"jsonrpc":"2.0","id":"version","method":"tasks/get","params":{"id":"t"}}"#,
                version,
            )
            .await;
            assert_eq!(response["error"]["code"], code, "{version:?}: {response}");
            assert_eq!(response["id"], "version");
        }
    }

    #[test]
    fn compat_blocking_normalizes_into_handler_hook() {
        harnx_core::require_nextest();
        for (configuration, expected) in [
            (json!({"blocking":true}), Ok(false)),
            (json!({"blocking":false}), Ok(true)),
            (
                json!({"blocking":true,"returnImmediately":false}),
                Ok(false),
            ),
            (json!({"blocking":false,"returnImmediately":true}), Ok(true)),
            (
                json!({"blocking":true,"returnImmediately":true}),
                Err(-32602),
            ),
            (
                json!({"blocking":false,"returnImmediately":false}),
                Err(-32602),
            ),
            (json!({"blocking":"true"}), Err(-32602)),
            (json!({"blocking":null}), Err(-32602)),
            (
                json!({"blocking":true,"return_immediately":false}),
                Ok(false),
            ),
            (
                json!({"blocking":true,"return_immediately":true}),
                Err(-32602),
            ),
            (
                json!({"blocking":true,"returnImmediately":false,"return_immediately":true}),
                Err(-32602),
            ),
            (
                json!({"blocking":true,"returnImmediately":"false"}),
                Err(-32602),
            ),
        ] {
            let mut request = json!({"method":"message/send","params":{"message":{"messageId":"m","role":"user","parts":[{"text":"hello"}]},"configuration":configuration}});
            let result = normalize_json(&mut request);
            match expected {
                Ok(immediate) => {
                    result.unwrap();
                    assert!(request["params"]["configuration"].get("blocking").is_none());
                    let decoded: a2a_lf::SendMessageRequest =
                        a2a_pb::protojson_conv::from_value(request["params"].clone()).unwrap();
                    assert_eq!(
                        crate::handler::return_immediately(decoded.configuration.as_ref()),
                        immediate
                    );
                }
                Err(code) => assert_eq!(result.unwrap_err().code, code),
            }
        }
    }

    #[tokio::test]
    async fn compat_body_limit_before_and_after_normalization() {
        let template = json!({"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{"messageId":"m","role":"user","parts":[{"text":""}]}}});
        for target_len in [MAX_REQUEST_BODY_BYTES + 1, MAX_REQUEST_BODY_BYTES] {
            let mut request = template.clone();
            request["params"]["message"]["parts"][0]["text"] =
                Value::String("x".repeat(target_len - template.to_string().len()));
            let body = request.to_string();
            assert_eq!(body.len(), target_len);
            // At the original limit, expanding user -> ROLE_USER crosses it.
            let response = app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/")
                        .header("a2a-version", "1.0")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        }
    }

    #[test]
    fn compat_lenient_enums_are_canonical_and_data_is_untouched() {
        harnx_core::require_nextest();
        for (input, canonical) in [
            ("user", "ROLE_USER"),
            ("ROLE_USER", "ROLE_USER"),
            ("agent", "ROLE_AGENT"),
            ("ROLE_AGENT", "ROLE_AGENT"),
            ("unspecified", "ROLE_UNSPECIFIED"),
        ] {
            let data = json!({"role":"user","state":"working","blocking":true});
            let mut request = json!({"method":"message/send","params":{"message":{"messageId":"m","role":input,"parts":[{"data":data}]}}});
            normalize_json(&mut request).unwrap();
            let decoded: a2a_lf::SendMessageRequest =
                a2a_pb::protojson_conv::from_value(request["params"].clone()).unwrap();
            let encoded = a2a_pb::protojson_conv::to_value(&decoded).unwrap();
            if canonical != "ROLE_UNSPECIFIED" {
                assert_eq!(encoded["message"]["role"], canonical);
            }
            assert_eq!(encoded["message"]["parts"][0]["data"], data);
        }
        for name in [
            "unspecified",
            "submitted",
            "working",
            "completed",
            "failed",
            "canceled",
            "input-required",
            "rejected",
            "auth-required",
        ] {
            let canonical = format!("TASK_STATE_{}", name.replace('-', "_").to_uppercase());
            for input in [name, canonical.as_str()] {
                let mut request = json!({"method":"ListTasks","params":{"status":input}});
                normalize_json(&mut request).unwrap();
                assert_eq!(request["params"]["status"], canonical);
                let decoded: a2a_lf::ListTasksRequest =
                    a2a_pb::protojson_conv::from_value(request["params"].clone()).unwrap();
                let encoded = a2a_pb::protojson_conv::to_value(&decoded).unwrap();
                if name == "unspecified" {
                    // ProtoJSON omits default-valued enums.
                    assert!(encoded.get("status").is_none());
                } else {
                    assert_eq!(encoded["status"], canonical);
                }
            }
        }
    }
}
