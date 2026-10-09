//! Request identity from trusted proxy-supplied headers or cookies, not authentication.
//!
//! A proxy must authenticate callers and strip or overwrite client-supplied identity
//! sources. Persist only the resolved user ID, never raw request headers or cookies.

use std::{fmt, str::FromStr};

use anyhow::{bail, Context};
use http::{header::COOKIE, HeaderMap, HeaderName};

mod membership;
pub use membership::MembershipHeaders;

/// First comma-separated entry of the first header field, trimmed.
///
/// Missing, empty or non-text headers return `None`. Identity resolution checks
/// presence separately so an unusable first source cannot trigger a fallback.
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
    Header(HeaderName),
    Cookie(String),
}

impl IdentitySource {
    /// Resolve this source only. Missing yields `None`; present but unusable fails closed.
    pub fn resolve<'a>(&self, headers: &'a HeaderMap) -> Result<Option<&'a str>, IdentityError> {
        match self {
            Self::Header(name) => {
                let Some(header) = headers.get(name) else {
                    return Ok(None);
                };
                let value = header.to_str().map_err(|_| IdentityError::InvalidValue)?;
                let value = value.split(',').next().unwrap_or_default().trim();
                if value.is_empty() {
                    Err(IdentityError::EmptyValue)
                } else {
                    Ok(Some(value))
                }
            }
            Self::Cookie(name) => cookie_value(headers, name),
        }
    }
}

impl FromStr for IdentitySource {
    type Err = anyhow::Error;

    /// Parse `header:NAME`, `cookie:NAME`, or bare `NAME` (a header).
    /// Prefixes are case-insensitive. Names must be nonempty HTTP tokens.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (cookie, name) = match raw.split_once(':') {
            Some((prefix, name)) if prefix.eq_ignore_ascii_case("header") => (false, name),
            Some((prefix, name)) if prefix.eq_ignore_ascii_case("cookie") => (true, name),
            Some(_) => {
                bail!("invalid identity source '{raw}': expected header:NAME or cookie:NAME")
            }
            None => (false, raw),
        };
        let header = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid identity source name '{name}'"))?;
        Ok(if cookie {
            Self::Cookie(name.to_owned())
        } else {
            Self::Header(header)
        })
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
    /// without considering later comma values, repeated fields or fallback sources.
    ///
    /// Headers retain `first_value` semantics (including HTTP horizontal tabs).
    /// Cookies use the first exact name match across all Cookie fields. Values
    /// may be quoted, must contain RFC 6265 cookie-octets, and aren't URL-decoded.
    /// Malformed unrelated cookies are ignored; a matching cookie without `=` is invalid.
    pub fn resolve(&self, headers: &HeaderMap) -> Result<Option<String>, IdentityError> {
        for source in &self.sources {
            if let Some(value) = source.resolve(headers)? {
                return Ok(Some(value.to_owned()));
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
        let header = IdentitySource::Header(HeaderName::from_static("x-user"));
        for raw in ["X-User", "header:X-User", "HeAdEr:X-User"] {
            assert_eq!(raw.parse::<IdentitySource>().unwrap(), header);
        }
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
    fn headers_keep_first_comma_and_repeated_field_semantics() {
        let sources = policy(&["X-USER"]);
        let mut headers = HeaderMap::new();
        headers.append("x-user", " \t first-user \t , second-user".parse().unwrap());
        headers.append("x-user", "third-user".parse().unwrap());
        assert_eq!(first_value(&headers, "x-user"), Some("first-user"));
        assert_eq!(
            sources.resolve(&headers).unwrap().as_deref(),
            Some("first-user")
        );
        assert_eq!(first_value(&headers, "absent"), None);
    }

    #[test]
    fn empty_or_invalid_header_fails_closed() {
        let sources = policy(&["x-user", "cookie:user", "x-fallback"]);
        for raw in [b"".as_slice(), b" \t ", b" , second", b",second", b"\xff"] {
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
}
