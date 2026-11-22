//! HTTP transport helpers for OAuth request building and response formatting.

use std::sync::Mutex;
use std::time::Duration;

use harnx_credentials::Secret;
use rmcp::transport::auth::{ClientRegistrationResponse, OAuthHttpClientError};
use serde_json::Value;

use super::{static_error, DEFAULT_TIMEOUT, STANDARD_OAUTH_ERRORS};

const MAX_ACCOUNT_LABEL_CHARS: usize = 200;

pub(crate) fn build_request(
    client: &reqwest::Client,
    request: oauth2::HttpRequest,
    url: url::Url,
    timeout: Option<Duration>,
) -> Result<reqwest::Request, OAuthHttpClientError> {
    let method = reqwest::Method::from_bytes(request.method().as_str().as_bytes())
        .map_err(|_| static_error("invalid http method"))?;

    let mut req_builder = client.request(method, url);
    for (name, value) in request.headers() {
        req_builder = req_builder.header(name, value);
    }
    req_builder = req_builder.body(request.body().clone());
    let effective_timeout = timeout
        .map(|t| t.min(DEFAULT_TIMEOUT))
        .unwrap_or(DEFAULT_TIMEOUT);
    req_builder = req_builder.timeout(effective_timeout);

    req_builder
        .build()
        .map_err(|_| static_error("failed to build oauth http request"))
}

pub(crate) async fn read_response_body(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, OAuthHttpClientError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| static_error("failed to read response chunk"))?
    {
        if body.len() + chunk.len() > limit {
            return Err(static_error("response body exceeded limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) async fn read_bounded_body(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ()> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len() + chunk.len() > limit {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) fn try_capture_dcr(
    body: &[u8],
    dcr_registration: &Mutex<Option<Secret<ClientRegistrationResponse>>>,
) {
    let Some(registration) = parse_dcr_registration(body) else {
        return;
    };
    if let Ok(mut slot) = dcr_registration.lock() {
        *slot = Some(registration);
    }
}

fn parse_dcr_registration(body: &[u8]) -> Option<Secret<ClientRegistrationResponse>> {
    serde_json::from_slice::<ClientRegistrationResponse>(body)
        .ok()
        .filter(|registration| !registration.client_id.trim().is_empty())
        .map(Secret::new)
}

pub(crate) fn build_success_response(
    response: reqwest::Response,
    body: Vec<u8>,
) -> Result<oauth2::HttpResponse, OAuthHttpClientError> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();

    let mut builder = oauth2::http::Response::builder()
        .status(status)
        .version(version);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        .map_err(|_| static_error("failed to build http response"))
}

pub(crate) fn build_error_response(
    response: reqwest::Response,
    body: &[u8],
) -> Result<oauth2::HttpResponse, OAuthHttpClientError> {
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    let sanitized_body = sanitize_oauth_error_body(body);

    let mut builder = oauth2::http::Response::builder()
        .status(status)
        .version(version);

    for (name, value) in &headers {
        if name != reqwest::header::CONTENT_LENGTH && name != reqwest::header::CONTENT_TYPE {
            builder = builder.header(name, value);
        }
    }
    builder = builder
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::CONTENT_LENGTH,
            sanitized_body.len().to_string(),
        );

    builder
        .body(sanitized_body)
        .map_err(|_| static_error("failed to build http error response"))
}

pub(crate) fn sanitize_oauth_error_body(raw: &[u8]) -> Vec<u8> {
    if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(raw) {
        if let Some(Value::String(err_code)) = map.get("error") {
            let trimmed = err_code.trim();
            if STANDARD_OAUTH_ERRORS.contains(&trimmed) {
                return serde_json::to_vec(&serde_json::json!({
                    "error": trimmed,
                    "error_description": "request failed"
                }))
                .unwrap_or_else(|_| {
                    br#"{"error":"server_error","error_description":"request failed"}"#.to_vec()
                });
            }
        }
    }
    br#"{"error":"server_error","error_description":"request failed"}"#.to_vec()
}

pub(crate) fn extract_account_label(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let raw_label = value
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| value.get("preferred_username").and_then(Value::as_str))
        .or_else(|| value.get("name").and_then(Value::as_str))?;

    let cleaned: String = raw_label
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_ACCOUNT_LABEL_CHARS)
        .collect();

    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
