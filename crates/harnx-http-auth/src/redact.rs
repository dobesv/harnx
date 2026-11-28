//! Secret redaction for upstream-sourced text. A provider can
//! echo the submitted credential back — in a JSON-RPC error, in tool result
//! content, in the error that fails a connection — and that text must never
//! reach a tool result, page or the journal as-is.

use reqwest::header::HeaderValue;
use std::borrow::Cow;

/// Placeholder that replaces a secret in redacted text.
const REDACTED: &str = "[redacted]";

/// Replace every occurrence of a known secret in `text` with
/// [`REDACTED`]. Secret-free text is returned borrowed.
pub fn redact_secrets<'a>(text: &'a str, secrets: &[String]) -> Cow<'a, str> {
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
pub fn redact_bearer<'a>(text: &'a str, header_value: Option<&HeaderValue>) -> Cow<'a, str> {
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

/// Redact raw, form-encoded (`+` for spaces), and percent-encoded variants
/// of known values. Handles the uppercase and lowercase hex spellings emitted
/// by encoders. Redact decoded JSON strings before serializing the result.
pub fn redact_secret_variants<'a>(text: &'a str, secrets: &[&str]) -> Cow<'a, str> {
    let mut forms = secrets
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty())
        .flat_map(secret_forms)
        .collect::<Vec<_>>();
    // Remove longer forms first so a raw substring can't hide a bigger match.
    forms.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    forms.dedup();
    redact_secrets(text, &forms)
}

fn secret_forms(secret: &str) -> [String; 5] {
    let form = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>();
    let percent = urlencoding::encode(secret).into_owned();
    let lower_form = lower_escape_hex(&form);
    let lower_percent = lower_escape_hex(&percent);
    [secret.to_owned(), form, percent, lower_form, lower_percent]
}

fn lower_escape_hex(encoded: &str) -> String {
    let mut out = encoded.to_owned();
    for (start, _) in encoded.match_indices('%') {
        out[start + 1..start + 3].make_ascii_lowercase();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_longest_raw_form_and_percent_variants() {
        let input = "plain value | token +é/& | token+%2B%C3%A9%2F%26 | token%20%2B%C3%A9%2F%26 | token+%2b%c3%a9%2f%26 | token%20%2b%c3%a9%2f%26";
        let actual = redact_secret_variants(input, &["token", "token +é/&", ""]);
        assert_eq!(
            actual,
            "plain value | [redacted] | [redacted] | [redacted] | [redacted] | [redacted]"
        );
        assert!(matches!(
            redact_secret_variants("keep me", &["a secret"]),
            Cow::Borrowed(_)
        ));
    }
}
