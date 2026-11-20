//! Request identity from trusted headers, cookies, or bearer claims, not authentication.
//!
//! A proxy must authenticate callers and strip or overwrite client-supplied identity
//! sources. Bearer decoding doesn't verify signatures; callers needing verification must
//! use `IdentitySources::select` and supply verified claims. Persist only the resolved
//! user ID, never raw request headers, cookies, or bearer tokens.

use std::{fmt, str::FromStr};

use anyhow::Context;
use http::{header::COOKIE, HeaderMap, HeaderName};

mod bearer;
mod header;
mod membership;
pub use bearer::BearerIdentity;
pub use header::HeaderSpec;
pub use membership::{merge_memberships, MembershipHeaders};

/// First comma-separated entry of the first header field, trimmed.
///
/// Missing, empty or non-text headers return `None`. Identity resolution checks
/// presence separately so an unusable first source cannot trigger a fallback.
///
/// Unlike `HeaderSpec::values`, the comma split here is deliberate: this is for
/// forwarded host/proto fields (`web_url.rs`), which proxies may comma-join when
/// aggregating repeated fields, so only the first entry is trusted. Identity
/// header sources must stay opaque (`header:`) or use `header-csv:` instead.
pub fn first_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// A request identity source. Cookie names are case-sensitive; header names aren't.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentitySource {
    Header(HeaderSpec),
    Cookie(String),
    ForgeFit,
    BearerClaim(String),
}

/// Ordered selection, before any bearer decoding or verification.
///
/// Deliberately doesn't implement `Debug`: the bearer branch contains a raw token.
pub enum IdentitySelection<'a> {
    Resolved(String),
    Bearer(BearerIdentity<'a>),
}

impl IdentitySelection<'_> {
    fn decode(self) -> Result<String, IdentityError> {
        match self {
            Self::Resolved(value) => Ok(value),
            Self::Bearer(bearer) => bearer.decode(),
        }
    }
}

impl IdentitySource {
    pub fn is_bearer(&self) -> bool {
        matches!(self, Self::ForgeFit | Self::BearerClaim(_))
    }

    /// Decode-only resolution. Bearer signatures and time/issuer/audience aren't checked.
    /// Use only behind a verifying proxy; use `select` to supply verified claims instead.
    pub fn resolve(&self, headers: &HeaderMap) -> Result<Option<String>, IdentityError> {
        self.select(headers)?
            .map(IdentitySelection::decode)
            .transpose()
    }

    /// Select this source without decoding bearer tokens. See `IdentitySources::select`.
    pub fn select<'a>(
        &'a self,
        headers: &'a HeaderMap,
    ) -> Result<Option<IdentitySelection<'a>>, IdentityError> {
        let value = match self {
            Self::Header(source) => {
                let Some(_) = headers.get(source.name()) else {
                    return Ok(None);
                };
                let value = source
                    .values(headers, header::HeaderFields::First)?
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                return if value.is_empty() {
                    Err(IdentityError::EmptyValue)
                } else {
                    Ok(Some(IdentitySelection::Resolved(value)))
                };
            }
            Self::Cookie(name) => cookie_value(headers, name),
            Self::ForgeFit | Self::BearerClaim(_) => {
                return bearer::select(self, headers)
                    .map(|value| value.map(IdentitySelection::Bearer));
            }
        }?;
        Ok(value.map(|value| IdentitySelection::Resolved(value.to_owned())))
    }
}

impl FromStr for IdentitySource {
    type Err = anyhow::Error;

    /// Parse `header:NAME`, `header-csv:NAME`, `cookie:NAME`, `forge-fit`, or
    /// `bearer-claim:CLAIM`.
    /// Bare names are headers, except `forge-fit` (use `header:forge-fit` for that header).
    /// Prefixes are case-insensitive. Header/cookie names must be HTTP tokens;
    /// claim names are nonempty, case-sensitive top-level JSON keys, not paths.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.eq_ignore_ascii_case("forge-fit") {
            return Ok(Self::ForgeFit);
        }
        match raw.split_once(':') {
            Some((prefix, name)) if prefix.eq_ignore_ascii_case("bearer-claim") => {
                anyhow::ensure!(!name.is_empty(), "empty bearer identity claim name");
                Ok(Self::BearerClaim(name.to_owned()))
            }
            Some((prefix, name)) if prefix.eq_ignore_ascii_case("cookie") => {
                HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid identity source name '{name}'"))?;
                Ok(Self::Cookie(name.to_owned()))
            }
            _ => raw.parse().map(Self::Header).with_context(|| format!("invalid identity source '{raw}': expected header:NAME, header-csv:NAME, cookie:NAME, forge-fit or bearer-claim:CLAIM")),
        }
    }
}

/// Ordered request identity policy. No sources, or no matching source, yields `None`.
#[derive(Clone, Debug, Default)]
pub struct IdentitySources {
    sources: Vec<IdentitySource>,
}

impl From<Vec<IdentitySource>> for IdentitySources {
    /// Build a policy from parsed sources, preserving their order.
    fn from(sources: Vec<IdentitySource>) -> Self {
        Self { sources }
    }
}

impl IdentitySources {
    /// Validate sources at startup, preserving configuration order.
    pub fn new(sources: &[String]) -> anyhow::Result<Self> {
        sources
            .iter()
            .map(|source| source.parse())
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Self::from)
    }

    pub fn sources(&self) -> &[IdentitySource] {
        &self.sources
    }

    /// The first present source wins. Empty or invalid values return an error,
    /// without considering repeated fields or fallback sources.
    ///
    /// Headers use only the first field. `header:` and bare names preserve the
    /// whole trimmed value; `header-csv:` selects the first parsed list item.
    /// Cookies use the first exact name match across all Cookie fields. Values
    /// may be quoted, must contain RFC 6265 cookie-octets, and aren't URL-decoded.
    /// Malformed unrelated cookies are ignored; a matching cookie without `=` is invalid.
    /// Bearer sources decode without verification; see `select` for verification plumbing.
    pub fn resolve(&self, headers: &HeaderMap) -> Result<Option<String>, IdentityError> {
        self.select(headers)?
            .map(IdentitySelection::decode)
            .transpose()
    }

    pub fn has_bearer_source(&self) -> bool {
        self.sources.iter().any(IdentitySource::is_bearer)
    }

    /// First-present-wins selection without decoding or verifying any bearer token.
    /// An earlier usable header/cookie returns `Resolved`, even with an invalid token.
    /// An unusable earlier source errors; missing/non-Bearer Authorization falls through.
    ///
    /// Verification-enabled callers must match `Bearer`, asynchronously verify `token()`,
    /// then pass only the verified JSON object to `BearerIdentity::resolve_claims`.
    /// Propagate verification/claim errors without retrying later sources. Never call
    /// decode-only `resolve` on a verification-enabled path. Both HTTP middleware and
    /// direct handler paths must follow this contract; no client-provided marker is proof
    /// of verification. Selection borrows the policy and headers across the verification.
    pub fn select<'a>(
        &'a self,
        headers: &'a HeaderMap,
    ) -> Result<Option<IdentitySelection<'a>>, IdentityError> {
        for source in &self.sources {
            if let Some(selection) = source.select(headers)? {
                return Ok(Some(selection));
            }
        }
        Ok(None)
    }
}

/// A configured source was present but unusable. Errors never contain identity values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityError {
    EmptyValue,
    InvalidValue,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyValue => "empty user identity source",
            Self::InvalidValue => "invalid user identity source",
        })
    }
}

impl std::error::Error for IdentityError {}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, IdentityError> {
    for header in headers.get_all(COOKIE) {
        // Inspect bytes so an unrelated non-text cookie doesn't hide a named source.
        for entry in header.as_bytes().split(|byte| *byte == b';') {
            let mut pair = entry.splitn(2, |byte| *byte == b'=');
            let key = pair.next().unwrap_or_default().trim_ascii();
            if key != name.as_bytes() {
                continue;
            }
            let raw = pair.next().ok_or(IdentityError::InvalidValue)?.trim_ascii();
            let value = unquote_cookie_value(raw);
            if value.is_empty() {
                return Err(IdentityError::EmptyValue);
            }
            if !is_valid_cookie_value(value) {
                return Err(IdentityError::InvalidValue);
            }
            return std::str::from_utf8(value)
                .map(Some)
                .map_err(|_| IdentityError::InvalidValue);
        }
    }
    Ok(None)
}

/// Remove surrounding double quotes from a cookie value if present.
fn unquote_cookie_value(raw: &[u8]) -> &[u8] {
    let quoted = raw.starts_with(b"\"") && raw.ends_with(b"\"") && raw.len() >= 2;
    if quoted {
        &raw[1..raw.len() - 1]
    } else {
        raw
    }
}

/// Check if a cookie value contains only characters allowed by RFC 6265.
/// Excludes controls, whitespace, quotes, comma, semicolon, and backslash.
fn is_valid_cookie_value(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| matches!(*byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn policy(sources: &[&str]) -> IdentitySources {
        harnx_core::require_nextest();
        IdentitySources::new(
            &sources
                .iter()
                .map(|source| (*source).to_owned())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn sources_parse_prefixes_and_preserve_cookie_case() {
        harnx_core::require_nextest();
        let header = IdentitySource::Header("x-user".parse().unwrap());
        for raw in ["X-User", "header:X-User", "HeAdEr:X-User"] {
            assert_eq!(raw.parse::<IdentitySource>().unwrap(), header);
        }
        let csv = "HeAdEr-CsV:X-User".parse::<IdentitySource>().unwrap();
        assert_eq!(
            csv,
            IdentitySource::Header("header-csv:x-user".parse().unwrap())
        );
        assert_ne!(csv, header);
        assert_eq!(
            "CoOkIe:User".parse::<IdentitySource>().unwrap(),
            IdentitySource::Cookie("User".into())
        );
    }

    #[test]
    fn sources_reject_empty_or_invalid_names_and_unknown_prefixes() {
        harnx_core::require_nextest();
        for raw in [
            "",
            "header:",
            "header-csv:",
            "header-csv:bad header",
            "header-csv:a:b",
            "cookie:",
            "bad header",
            "user:id",
            "header:a:b",
            "cookie:a:b",
            "cookie:a=b",
            "cookie:a;b",
            "cookie: user",
            "cookie:user\n",
            "header:user\n",
            "query:user",
        ] {
            assert!(raw.parse::<IdentitySource>().is_err(), "{raw:?}");
            assert!(IdentitySources::new(&[raw.into()]).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn missing_sources_allow_caller_defaults() {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, "unrelated=value".parse().unwrap());
        headers.insert("x-user", "ignored".parse().unwrap());
        assert_eq!(policy(&[]).resolve(&headers).unwrap(), None);
        assert_eq!(
            policy(&["x-other", "cookie:user"])
                .resolve(&headers)
                .unwrap(),
            None
        );
    }

    #[test]
    fn mixed_sources_use_configured_precedence() {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, "user=cookie-user".parse().unwrap());
        headers.insert("x-user", "header-user".parse().unwrap());
        for (sources, expected) in [
            (vec!["x-user", "cookie:user"], "header-user"),
            (vec!["cookie:user", "x-user"], "cookie-user"),
            (vec!["x-absent", "cookie:user", "x-user"], "cookie-user"),
            (vec!["cookie:absent", "x-user"], "header-user"),
        ] {
            assert_eq!(
                policy(&sources).resolve(&headers).unwrap().as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn headers_keep_commas_and_use_first_repeated_field() {
        let sources = policy(&["X-USER"]);
        let mut headers = HeaderMap::new();
        headers.append(
            "x-user",
            " \t first-user \t , second-user \t ".parse().unwrap(),
        );
        headers.append("x-user", "third-user".parse().unwrap());
        assert_eq!(first_value(&headers, "x-user"), Some("first-user"));
        assert_eq!(
            sources.resolve(&headers).unwrap().as_deref(),
            Some("first-user \t , second-user")
        );
        assert_eq!(first_value(&headers, "absent"), None);
    }

    #[test]
    fn csv_identity_uses_first_item_and_fails_closed_on_empty_or_malformed_lists() {
        let sources = policy(&["HeAdEr-CsV:X-User", "x-fallback"]);
        for (raw, expected) in [
            (" first-user , second-user ", "first-user"),
            ("CN=alice,OU=eng,O=Acme", "CN=alice"),
            (
                r#""CN=alice,OU=eng,O=Acme", other"#,
                "CN=alice,OU=eng,O=Acme",
            ),
            (r"CN=Smith\, Alice, second", r"CN=Smith\, Alice"),
        ] {
            let mut headers = HeaderMap::new();
            headers.append("x-user", raw.parse().unwrap());
            headers.append("x-user", "ignored".parse().unwrap());
            assert_eq!(
                sources.resolve(&headers).unwrap().as_deref(),
                Some(expected)
            );
        }
        for raw in ["", " \t ", ",later", " ,later", r#""",later"#] {
            let mut headers = HeaderMap::new();
            headers.append("x-user", raw.parse().unwrap());
            headers.append("x-user", "ignored".parse().unwrap());
            headers.insert("x-fallback", "fallback".parse().unwrap());
            assert!(sources.resolve(&headers).is_err());
        }
        for raw in [
            r#""unterminated"#,
            r#"valid, "unterminated"#,
            r#""closed"junk, later"#,
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-user", raw.parse().unwrap());
            headers.insert("x-fallback", "fallback".parse().unwrap());
            assert_eq!(sources.resolve(&headers), Err(IdentityError::InvalidValue));
        }
    }

    #[test]
    fn headers_preserve_distinguished_names_including_escaped_commas() {
        for source in [
            "header:X-Amzn-Mtls-Clientcert-Subject",
            "X-Amzn-Mtls-Clientcert-Subject",
        ] {
            let sources = policy(&[source]);
            for dn in [
                "CN=alice,OU=eng,O=Acme Corp",
                r"CN=Smith\, Alice,OU=eng",
                "O=Acme Corp,OU=eng,CN=alice",
            ] {
                let mut headers = HeaderMap::new();
                headers.append(
                    "x-amzn-mtls-clientcert-subject",
                    format!(" \t {dn} \t ").parse().unwrap(),
                );
                headers.append("x-amzn-mtls-clientcert-subject", "ignored".parse().unwrap());
                assert_eq!(sources.resolve(&headers).unwrap().as_deref(), Some(dn));
            }
        }
    }

    #[test]
    fn headers_accept_nonempty_comma_values_as_opaque_identities() {
        let sources = policy(&["x-user"]);
        for raw in [",", " , second", ",second"] {
            let mut headers = HeaderMap::new();
            headers.insert("x-user", raw.parse().unwrap());
            assert_eq!(
                sources.resolve(&headers).unwrap().as_deref(),
                Some(raw.trim())
            );
            assert_eq!(first_value(&headers, "x-user"), None);
        }
    }

    #[test]
    fn opaque_identity_headers_do_not_decode_list_quoting() {
        let sources = policy(&["x-user"]);
        let raw = r#""CN=Smith\, Alice,OU=eng""#;
        let mut headers = HeaderMap::new();
        headers.insert("x-user", raw.parse().unwrap());
        assert_eq!(sources.resolve(&headers).unwrap().as_deref(), Some(raw));
    }

    #[test]
    fn empty_or_invalid_header_fails_closed() {
        let sources = policy(&["x-user", "cookie:user", "x-fallback"]);
        for raw in [b"".as_slice(), b" \t ", b"\xff"] {
            let mut headers = HeaderMap::new();
            headers.append("x-user", HeaderValue::from_bytes(raw).unwrap());
            headers.append("x-user", "later-user".parse().unwrap());
            headers.insert("x-fallback", "fallback".parse().unwrap());
            headers.insert(COOKIE, "user=cookie-user".parse().unwrap());
            assert!(sources.resolve(&headers).is_err(), "{raw:?}");
            assert_eq!(first_value(&headers, "x-user"), None);
        }
    }

    #[test]
    fn cookie_names_require_an_exact_match() {
        let policy = policy(&["cookie:user"]);
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("user_id=wrong; username=wrong"),
        );
        assert_eq!(policy.resolve(&headers), Ok(None));
        headers.append(COOKIE, HeaderValue::from_static("user=right"));
        assert_eq!(policy.resolve(&headers), Ok(Some("right".into())));
    }

    #[test]
    fn cookies_parse_pairs_quotes_and_equals_without_decoding() {
        let sources = policy(&["cookie:User"]);
        for (raw, expected) in [
            ("a=b; User=user-id; c=d", "user-id"),
            (" a=b ; User = \"user-id\" ; c=d ", "user-id"),
            ("User=user=id==", "user=id=="),
            ("User=user%40example.com", "user%40example.com"),
            ("user=wrong-case; User=right-case", "right-case"),
            ("unrelated; User=valid", "valid"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(COOKIE, raw.parse().unwrap());
            assert_eq!(
                sources.resolve(&headers).unwrap().as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn cookies_search_multiple_fields_and_take_first_duplicate() {
        let sources = policy(&["cookie:user"]);
        let mut headers = HeaderMap::new();
        headers.append(COOKIE, "a=b".parse().unwrap());
        headers.append(COOKIE, "user=first; user=second".parse().unwrap());
        headers.append(COOKIE, "user=third".parse().unwrap());
        assert_eq!(sources.resolve(&headers).unwrap().as_deref(), Some("first"));
        headers.insert(
            COOKIE,
            HeaderValue::from_bytes(b"unrelated=\xff; user=valid").unwrap(),
        );
        assert_eq!(sources.resolve(&headers).unwrap().as_deref(), Some("valid"));

        headers.insert(COOKIE, "fallback=lower-priority".parse().unwrap());
        headers.append(COOKIE, "primary=higher-priority".parse().unwrap());
        assert_eq!(
            policy(&["cookie:primary", "cookie:fallback"])
                .resolve(&headers)
                .unwrap()
                .as_deref(),
            Some("higher-priority")
        );
    }

    #[test]
    fn empty_or_malformed_cookie_fails_closed_across_fields_and_sources() {
        let sources = policy(&["cookie:user", "x-fallback"]);
        for raw in [
            b"user=".as_slice(),
            b"user= \t ",
            b"user=\"\"",
            b"user=; user=later-user",
            b"user=\"\"; user=later-user",
            b"user",
            b"user=\"bad",
            b"user=bad\"",
            b"user=bad value",
            b"user=bad\tvalue",
            b"user=a,b",
            b"user=bad\\value",
            b"user=\xff",
        ] {
            let mut headers = HeaderMap::new();
            headers.append(COOKIE, HeaderValue::from_bytes(raw).unwrap());
            headers.append(COOKIE, "user=later-user".parse().unwrap());
            headers.insert("x-fallback", "fallback".parse().unwrap());
            assert!(sources.resolve(&headers).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn successful_source_ignores_invalid_lower_priority_sources() {
        let mut headers = HeaderMap::new();
        headers.insert("x-user", "primary".parse().unwrap());
        headers.insert(COOKIE, "user=".parse().unwrap());
        assert_eq!(
            policy(&["x-user", "cookie:user"])
                .resolve(&headers)
                .unwrap()
                .as_deref(),
            Some("primary")
        );
    }

    fn payload_token(payload: &[u8]) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(payload))
    }

    fn bearer_headers(claims: serde_json::Value) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let token = payload_token(&serde_json::to_vec(&claims).unwrap());
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    fn assert_bearer_failure(source: &str, headers: &HeaderMap) {
        let mut headers = headers.clone();
        headers.append("authorization", "Bearer later.valid.token".parse().unwrap());
        headers.insert("x-fallback", "fallback-identity".parse().unwrap());
        let error = policy(&[source, "x-fallback"])
            .resolve(&headers)
            .unwrap_err();
        // Exact messages also prove no decoder, token, claim, or identity text leaks.
        match error {
            IdentityError::EmptyValue => {
                assert_eq!(error.to_string(), "empty user identity source");
                assert_eq!(format!("{error:?}"), "EmptyValue");
            }
            IdentityError::InvalidValue => {
                assert_eq!(error.to_string(), "invalid user identity source");
                assert_eq!(format!("{error:?}"), "InvalidValue");
            }
        }
    }

    #[test]
    fn bearer_sources_parse_and_report_presence() {
        for raw in ["forge-fit", "FoRgE-FiT"] {
            assert_eq!(
                raw.parse::<IdentitySource>().unwrap(),
                IdentitySource::ForgeFit
            );
        }
        assert_eq!(
            "BeArEr-ClAiM:Principal".parse::<IdentitySource>().unwrap(),
            IdentitySource::BearerClaim("Principal".into())
        );
        assert_eq!(
            "header:forge-fit".parse::<IdentitySource>().unwrap(),
            IdentitySource::Header("forge-fit".parse().unwrap())
        );
        assert!("bearer-claim:".parse::<IdentitySource>().is_err());
        assert!("forge-fit:principal".parse::<IdentitySource>().is_err());
        assert!(policy(&["forge-fit"]).has_bearer_source());
        assert!(policy(&["cookie:user", "bearer-claim:principal"]).has_bearer_source());
        assert!(!policy(&["header:forge-fit", "cookie:user"]).has_bearer_source());
        assert!(!policy(&[]).has_bearer_source());
        let error = "unknown:source"
            .parse::<IdentitySource>()
            .unwrap_err()
            .to_string();
        assert!(error.contains(
            "header:NAME, header-csv:NAME, cookie:NAME, forge-fit or bearer-claim:CLAIM"
        ));
    }

    #[test]
    fn forge_fit_builds_cn_first_dn_and_org_only_identity() {
        use serde_json::json;
        let sources = policy(&["forge-fit"]);
        let headers = bearer_headers(json!({
            "principal": "655362:312d3308-8954-42b0-aa38-771a10c88656",
            "context": {"cloudId": "d0d52620-3203-4cfa-8db5-f2587155f0dd"}
        }));
        assert_eq!(sources.resolve(&headers).unwrap().as_deref(), Some(
            "CN=655362:312d3308-8954-42b0-aa38-771a10c88656,OU=d0d52620-3203-4cfa-8db5-f2587155f0dd,O=atlassian.com"
        ));
        for claims in [
            json!({"context": {"cloudId": "site-id"}}),
            json!({"principal": "unidentified", "context": {"cloudId": "site-id"}}),
        ] {
            assert_eq!(
                sources.resolve(&bearer_headers(claims)).unwrap().as_deref(),
                Some("OU=site-id,O=atlassian.com")
            );
        }
    }

    #[test]
    fn bearer_missing_or_basic_authorization_falls_through() {
        for source in ["forge-fit", "bearer-claim:principal"] {
            let sources = policy(&[source, "x-fallback"]);
            let mut headers = HeaderMap::new();
            headers.insert("x-fallback", "fallback".parse().unwrap());
            for authorization in [
                None,
                Some("Basic credentials"),
                Some("Bearerish token"),
                Some(""),
            ] {
                headers.remove("authorization");
                if let Some(value) = authorization {
                    headers.insert("authorization", value.parse().unwrap());
                }
                assert_eq!(
                    sources.resolve(&headers).unwrap().as_deref(),
                    Some("fallback")
                );
                assert_eq!(policy(&[source]).resolve(&headers).unwrap(), None);
            }
        }
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_and_token_whitespace_is_trimmed() {
        let token = payload_token(br#"{"principal":"account-id","context":{"cloudId":"site-id"}}"#);
        for scheme in ["bearer", "Bearer", "BEARER", "bEaReR"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "authorization",
                format!(" {scheme}\t  {token} \t").parse().unwrap(),
            );
            assert_eq!(
                policy(&["forge-fit"]).resolve(&headers).unwrap().as_deref(),
                Some("CN=account-id,OU=site-id,O=atlassian.com")
            );
        }
    }

    #[test]
    fn bearer_malformed_tokens_fail_closed_without_leaking_data() {
        let mut tokens = vec![
            "secret-token".to_owned(),
            "secret.token".into(),
            "a.b.c.d".into(),
            ".e30.signature".into(),
            "header..signature".into(),
            "header.e30.".into(),
            "header.%%%secret-payload.signature".into(),
            "header.eyJ9=.signature".into(),
        ];
        for payload in [
            b"not-json-secret".as_slice(),
            b"[]",
            b"null",
            b"42",
            b"\"secret-string\"",
            b"\xff",
        ] {
            tokens.push(payload_token(payload));
        }
        for source in ["forge-fit", "bearer-claim:principal"] {
            for token in &tokens {
                let mut headers = HeaderMap::new();
                headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
                assert_bearer_failure(source, &headers);
            }
            for raw in [b"Bearer".as_slice(), b"Bearer \t", b"Bearer \xff"] {
                let mut headers = HeaderMap::new();
                headers.insert("authorization", HeaderValue::from_bytes(raw).unwrap());
                assert_bearer_failure(source, &headers);
            }
        }
    }

    #[test]
    fn forge_fit_missing_or_invalid_context_fails_closed() {
        use serde_json::json;
        for claims in [
            json!({"principal": "secret-account"}),
            json!({"context": null}),
            json!({"context": "secret-context"}),
            json!({"context": []}),
            json!({"context": {}}),
            json!({"context": {"cloudId": null}}),
            json!({"context": {"cloudId": 123}}),
            json!({"context": {"cloudId": false}}),
            json!({"context": {"cloudId": []}}),
            json!({"context": {"cloudId": {}}}),
            json!({"context": {"cloudId": ""}}),
        ] {
            assert_bearer_failure("forge-fit", &bearer_headers(claims));
        }
    }

    #[test]
    fn forge_fit_present_nonstring_or_empty_principal_fails_closed() {
        use serde_json::json;
        for principal in [
            json!(null),
            json!(123),
            json!(false),
            json!([]),
            json!({}),
            json!(""),
        ] {
            assert_bearer_failure(
                "forge-fit",
                &bearer_headers(json!({
                    "principal": principal, "context": {"cloudId": "secret-site"}
                })),
            );
        }
    }

    #[test]
    fn forge_fit_rejects_ambiguous_dn_components_in_principal_and_cloud_id() {
        use serde_json::json;
        let mut invalid = vec![
            " secret".to_owned(),
            "secret ".into(),
            "\u{2003}secret".into(),
            "secret\u{2003}".into(),
        ];
        for ch in [
            ',', '=', '+', '<', '>', '#', ';', '"', '\\', '\0', '\n', '\r', '\t', '\u{7f}',
            '\u{85}',
        ] {
            invalid.push(format!("secret{ch}component"));
        }
        for value in invalid {
            for claims in [
                json!({"principal": value, "context": {"cloudId": "secret-site"}}),
                json!({"principal": "secret-account", "context": {"cloudId": value}}),
            ] {
                assert_bearer_failure("forge-fit", &bearer_headers(claims));
            }
        }
    }

    #[test]
    fn bearer_claim_returns_trimmed_bare_account_id_without_comma_splitting() {
        use serde_json::json;
        for (value, expected) in [
            ("655362:account-id", "655362:account-id"),
            ("  account-id  ", "account-id"),
            ("\u{2003}account-id\u{2003}", "account-id"),
            (
                "CN=account,OU=site,O=atlassian.com",
                "CN=account,OU=site,O=atlassian.com",
            ),
        ] {
            let headers = bearer_headers(json!({"principal": value}));
            assert_eq!(
                policy(&["bearer-claim:principal"])
                    .resolve(&headers)
                    .unwrap()
                    .as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn bearer_claim_missing_nonstring_empty_or_control_values_fail_closed() {
        use serde_json::json;
        assert_bearer_failure(
            "bearer-claim:principal",
            &bearer_headers(json!({"Principal": "secret"})),
        );
        for value in [
            json!(null),
            json!(123),
            json!(false),
            json!([]),
            json!({}),
            json!(""),
            json!("   "),
            json!("secret\nclaim"),
            json!("secret\0claim"),
            json!("secret\u{85}claim"),
            json!("\tsecret"),
        ] {
            assert_bearer_failure(
                "bearer-claim:principal",
                &bearer_headers(json!({"principal": value})),
            );
        }
    }

    #[test]
    fn bearer_claim_uses_literal_top_level_keys_not_nested_paths() {
        use serde_json::json;
        let headers =
            bearer_headers(json!({"context.cloudId": "literal", "context": {"cloudId": "nested"}}));
        assert_eq!(
            policy(&["bearer-claim:context.cloudId"])
                .resolve(&headers)
                .unwrap()
                .as_deref(),
            Some("literal")
        );
        assert_bearer_failure(
            "bearer-claim:context.cloudId",
            &bearer_headers(json!({"context": {"cloudId": "nested"}})),
        );
    }

    #[test]
    fn bearer_resolution_preserves_order_and_never_decodes_lower_priority_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert("x-user", "header-user".parse().unwrap());
        headers.insert("cookie", "user=cookie-user".parse().unwrap());
        headers.insert(
            "authorization",
            "Bearer invalid-secret-token".parse().unwrap(),
        );
        assert_eq!(
            policy(&["x-user", "forge-fit"])
                .resolve(&headers)
                .unwrap()
                .as_deref(),
            Some("header-user")
        );
        assert_eq!(
            policy(&["cookie:user", "forge-fit"])
                .resolve(&headers)
                .unwrap()
                .as_deref(),
            Some("cookie-user")
        );
        assert_bearer_failure("forge-fit", &headers);
        headers.insert("x-user", "".parse().unwrap());
        assert_eq!(
            policy(&["x-user", "forge-fit"]).resolve(&headers),
            Err(IdentityError::EmptyValue)
        );
    }

    #[test]
    fn bearer_selection_maps_only_supplied_verified_claims() {
        use serde_json::json;
        let headers = bearer_headers(
            json!({"principal": "unverified-account", "context": {"cloudId": "unverified-site"}}),
        );
        for (source, expected) in [
            (
                "forge-fit",
                "CN=verified-account,OU=verified-site,O=atlassian.com",
            ),
            ("bearer-claim:principal", "verified-account"),
        ] {
            let sources = policy(&[source, "x-fallback"]);
            let Some(IdentitySelection::Bearer(bearer)) = sources.select(&headers).unwrap() else {
                panic!("expected bearer selection")
            };
            assert!(bearer.token().starts_with("e30."));
            let claims =
                json!({"principal": "verified-account", "context": {"cloudId": "verified-site"}});
            assert_eq!(
                bearer.resolve_claims(claims.as_object().unwrap()).unwrap(),
                expected
            );
            assert_eq!(
                bearer.resolve_claims(json!({}).as_object().unwrap()),
                Err(IdentityError::InvalidValue)
            );
        }
    }

    #[test]
    fn bearer_selection_stops_before_verification_and_respects_precedence() {
        let mut headers = HeaderMap::new();
        headers.insert("x-user", "header-user".parse().unwrap());
        headers.insert("authorization", "Bearer not-even-a-jwt".parse().unwrap());
        let sources = policy(&["x-user", "forge-fit"]);
        assert!(
            matches!(sources.select(&headers).unwrap(), Some(IdentitySelection::Resolved(value)) if value == "header-user")
        );
        let sources = policy(&["forge-fit", "x-user"]);
        let Some(IdentitySelection::Bearer(bearer)) = sources.select(&headers).unwrap() else {
            panic!("expected bearer selection")
        };
        assert_eq!(bearer.token(), "not-even-a-jwt");
        headers.insert("x-user", "".parse().unwrap());
        let sources = policy(&["x-user", "forge-fit"]);
        assert!(matches!(
            sources.select(&headers),
            Err(IdentityError::EmptyValue)
        ));
    }
}
