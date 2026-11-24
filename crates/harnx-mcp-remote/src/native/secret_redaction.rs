//! Secret redaction for upstream-sourced text. A provider can
//! echo the submitted credential back — in a JSON-RPC error, in tool result
//! content, in the error that fails a connection — and that text must never
//! reach a tool result, page or the journal as-is.

use reqwest::header::HeaderValue;
use std::borrow::Cow;

/// Placeholder that replaces a secret in redacted text.
pub(crate) const REDACTED: &str = "[redacted]";

/// Replace every occurrence of a known secret in `text` with
/// [`REDACTED`]. Secret-free text is returned borrowed.
pub(crate) fn redact_secrets<'a>(text: &'a str, secrets: &[String]) -> Cow<'a, str> {
    let mut owned: Option<String> = None;
    for secret in secrets {
        if secret.is_empty() || !text.contains(secret.as_str()) {
            continue;
        }
        let current = owned.as_deref().unwrap_or(text);
        owned = Some(current.replace(secret.as_str(), REDACTED));
    }
    owned.map_or_else(|| Cow::Borrowed(text), Cow::Owned)
}

/// Redact the secret forms carried by one `Authorization` header value: the
/// full value and, with a `Bearer` scheme, the bare token after it.
pub(crate) fn redact_bearer<'a>(text: &'a str, header_value: Option<&HeaderValue>) -> Cow<'a, str> {
    match header_value.and_then(|value| value.to_str().ok()) {
        Some(value) => {
            let mut secrets = vec![value.to_string()];
            if let Some(token) = value.strip_prefix("Bearer ") {
                secrets.push(token.to_string());
            }
            redact_secrets(text, &secrets)
        }
        None => Cow::Borrowed(text),
    }
}
