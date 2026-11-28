//! Pure token-response parsing and validation, plus the request form and
//! safe diagnostics for one client-credentials exchange. Everything here is
//! side-effect free; the state machine lives in the parent module.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use reqwest::header::HeaderValue;
use serde_json::Value;
use tokio::time::Instant;
use url::form_urlencoded;

use crate::native::oauth_http::STANDARD_OAUTH_ERRORS;

/// Read one token-response body up to `limit`. Unlike the native
/// best-effort reader, a mid-body read failure is a failure: chunk errors
/// propagate, so a truncated or reset connection can never be mistaken
/// for a complete payload and no partial body is ever returned to the
/// caller.
#[derive(Debug, Clone, Copy)]
pub(super) enum BodyReadError {
    Read,
    TooLarge,
}

pub(super) async fn read_token_body(
    response: &mut reqwest::Response,
    limit: usize,
) -> std::result::Result<Vec<u8>, BodyReadError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| BodyReadError::Read)? {
        if body.len() + chunk.len() > limit {
            return Err(BodyReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Refresh when this much or less time remains before expiry.
pub(super) const PRE_EXPIRY_WINDOW: Duration = Duration::from_secs(60);

/// One cached token: its installed `Authorization` header, the redactable
/// forms of its secret, and its monotonic expiry.
#[derive(Clone)]
pub(super) struct CachedToken {
    pub(super) header: HeaderValue,
    pub(super) secrets: Vec<String>,
    /// `None` means the response carried no `expires_in`: refresh only
    /// after a rejection.
    pub(super) expires_at: Option<Instant>,
}

pub(super) fn is_fresh(token: &CachedToken, now: Instant) -> bool {
    let Some(expires_at) = token.expires_at else {
        return true;
    };
    match now.checked_add(PRE_EXPIRY_WINDOW) {
        Some(window) => expires_at > window,
        None => false,
    }
}

pub(super) fn form_body(client_id: &str, client_secret: &str, scopes: &[String]) -> String {
    let mut form = form_urlencoded::Serializer::new(String::new());
    form.extend_pairs([
        ("grant_type", "client_credentials"),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ]);
    if !scopes.is_empty() {
        form.append_pair("scope", &scopes.join(" "));
    }
    form.finish()
}

/// Bounded, redacted non-success diagnostic: the HTTP status plus at most
/// one allowlisted standard OAuth error code. Provider prose, URLs, token
/// fields and malformed bodies never appear.
pub(super) fn status_error(status: reqwest::StatusCode, body: &[u8]) -> String {
    match allowlisted_code(body) {
        Some(code) => format!("token endpoint returned HTTP {status} ({code})"),
        None => format!("token endpoint returned HTTP {status}"),
    }
}

pub(super) fn status_reason(status: reqwest::StatusCode, reason: &'static str) -> String {
    format!("token endpoint returned HTTP {status}; {reason}")
}

pub(super) fn body_read_error(status: reqwest::StatusCode, error: BodyReadError) -> String {
    let reason = match error {
        BodyReadError::Read => "response body could not be read",
        BodyReadError::TooLarge => "response body exceeded the size limit",
    };
    if status.is_success() {
        format!("token {reason}")
    } else {
        status_reason(status, reason)
    }
}

pub(super) fn body_timeout_error(status: reqwest::StatusCode) -> String {
    if status.is_success() {
        "token exchange timed out".to_owned()
    } else {
        status_reason(status, "response body timed out")
    }
}

/// RFC 6750 `b64token`: one or more token characters, then optional `=` padding.
fn is_bearer_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    let padding_start = bytes
        .iter()
        .position(|byte| *byte == b'=')
        .unwrap_or(bytes.len());
    padding_start > 0
        && bytes[..padding_start].iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
        && bytes[padding_start..].iter().all(|byte| *byte == b'=')
}

fn allowlisted_code(body: &[u8]) -> Option<&'static str> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let raw = value.get("error")?.as_str()?.trim();
    STANDARD_OAUTH_ERRORS
        .iter()
        .copied()
        .find(|code| *code == raw)
}

/// Strict token-response validation with static parse errors only. The
/// cache is published only after every check passes; a returned
/// refresh token is ignored.
pub(super) fn parse_token_response(body: &[u8]) -> Result<CachedToken> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| anyhow!("token response is not valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("token response is not a JSON object"))?;
    let token = object
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token response has no access_token"))?;
    if token.trim().is_empty() {
        bail!("token response has an empty access_token");
    }
    if !is_bearer_token(token) {
        bail!("access_token is not a valid header value");
    }
    let token_type = object
        .get("token_type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token response has no token_type"))?;
    if !token_type.trim().eq_ignore_ascii_case("bearer") {
        bail!("token response token_type is not Bearer");
    }
    // `as_u64` rejects fractions, negatives and strings; a value that fits
    // u64 but overflows the clock on addition is rejected just as strictly.
    let expires_in = match object.get("expires_in") {
        None => None,
        Some(raw) => Some(
            raw.as_u64()
                .ok_or_else(|| anyhow!("token response has an invalid expires_in"))?,
        ),
    };
    let now = Instant::now();
    let expires_at = match expires_in {
        None => None,
        Some(secs) => Some(
            now.checked_add(Duration::from_secs(secs))
                .ok_or_else(|| anyhow!("token response expires_in exceeds the clock range"))?,
        ),
    };
    let raw = token.to_owned();
    let mut header = HeaderValue::from_str(&format!("Bearer {raw}"))
        .map_err(|_| anyhow!("access_token is not a valid header value"))?;
    header.set_sensitive(true);
    Ok(CachedToken {
        header,
        secrets: vec![raw.clone(), format!("Bearer {raw}")],
        expires_at,
    })
}
