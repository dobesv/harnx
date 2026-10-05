//! Addresses of sessions in the Web UI.
//!
//! A browser opens a session at `{base}/agents/{agent}/sessions/{session}`.
//! The base is the configured `serve_public_url` when there is one. Otherwise
//! it is inferred from the request that carried the prompt: the host a
//! reverse proxy forwarded, or else the host the request named, with the
//! scheme the proxy forwarded. A client can send those headers itself when
//! no proxy overwrites them, so an inferred address is only a hint, and a
//! configured one replaces it.

use crate::session_actor::SessionKey;
use anyhow::{ensure, Context, Result};
use harnx_runtime::config::Config;
use http::{HeaderMap, Uri};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

/// What `encodeURIComponent` leaves unescaped, so an address built here is
/// the one the Web UI builds for the same session.
const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// The configured `serve_public_url`, checked and without a trailing slash.
/// It may carry a path when the Web UI is served under a prefix.
pub(crate) fn public_url(config: &Config) -> Result<Option<String>> {
    normalize_public_url(config.serve_public_url.as_deref())
}

/// [`public_url`] for a server built without `run`'s startup check: an
/// invalid value is logged and ignored, leaving addresses to be inferred.
pub(crate) fn public_url_or_warn(config: &Config) -> Option<String> {
    public_url(config).unwrap_or_else(|error| {
        log::warn!("ignoring serve_public_url: {error:#}");
        None
    })
}

fn normalize_public_url(raw: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let url = reqwest::Url::parse(raw)
        .with_context(|| format!("serve_public_url '{raw}' is not a URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "serve_public_url '{raw}' must be an http or https URL without credentials, query or fragment"
    );
    Ok(Some(url.as_str().trim_end_matches('/').to_string()))
}

/// The base URL a request reached this server at: the host and scheme a
/// reverse proxy forwarded, or the request's own host over plain http.
pub(crate) fn inferred_base_url(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let host = first_value(headers, "x-forwarded-host")
        .or_else(|| first_value(headers, http::header::HOST.as_str()))
        .or_else(|| uri.authority().map(|authority| authority.as_str()))?;
    let authority = host.parse::<http::uri::Authority>().ok()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let scheme = match first_value(headers, "x-forwarded-proto") {
        Some(proto) if proto.eq_ignore_ascii_case("https") => "https",
        _ => "http",
    };
    Some(format!("{scheme}://{authority}"))
}

/// The first entry of a header that proxies may append to, comma-separated.
fn first_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The address that opens `key`'s session in the Web UI served at `base`,
/// naming the agent as the Web UI does: without the cluster suffix when the
/// agent is on `default_cluster`.
pub(crate) fn session_url(base: &str, key: &SessionKey, default_cluster: Option<&str>) -> String {
    let agent = key
        .target()
        .display_ref_with_default_cluster(default_cluster);
    format!(
        "{base}/agents/{}/sessions/{}",
        utf8_percent_encode(&agent, URI_COMPONENT),
        utf8_percent_encode(key.session(), URI_COMPONENT)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    fn base(pairs: &[(&'static str, &'static str)]) -> Option<String> {
        inferred_base_url(&headers(pairs), &Uri::from_static("/agents/a/sessions/s"))
    }

    fn configured(value: &str) -> Result<Option<String>> {
        normalize_public_url(Some(value))
    }

    #[test]
    fn forwarded_host_and_scheme_win_over_the_host_header() {
        assert_eq!(
            base(&[
                ("host", "harnx-serve.internal:8000"),
                ("x-forwarded-host", "harnx.example.com, proxy.internal"),
                ("x-forwarded-proto", "HTTPS, http"),
            ])
            .as_deref(),
            Some("https://harnx.example.com")
        );
        assert_eq!(
            base(&[
                ("host", "harnx.example.com"),
                ("x-forwarded-proto", "https")
            ])
            .as_deref(),
            Some("https://harnx.example.com")
        );
        assert_eq!(
            base(&[("host", "127.0.0.1:8000")]).as_deref(),
            Some("http://127.0.0.1:8000")
        );
    }

    #[test]
    fn unusable_hosts_infer_nothing() {
        assert_eq!(base(&[]), None);
        assert_eq!(base(&[("host", "user@harnx.example.com")]), None);
        assert_eq!(base(&[("host", "harnx.example.com/agents")]), None);
        assert_eq!(
            inferred_base_url(
                &HeaderMap::new(),
                &Uri::from_static("http://harnx.example.com:8443/agents")
            )
            .as_deref(),
            Some("http://harnx.example.com:8443"),
            "an absolute request URI still names its host"
        );
    }

    #[test]
    fn configured_public_url_is_checked_and_normalized() {
        assert_eq!(configured("  ").unwrap(), None);
        assert_eq!(
            configured("https://Harnx.Example.com/").unwrap().as_deref(),
            Some("https://harnx.example.com")
        );
        assert_eq!(
            configured("https://example.com/harnx/").unwrap().as_deref(),
            Some("https://example.com/harnx")
        );
        for invalid in [
            "harnx.example.com",
            "ftp://harnx.example.com",
            "https://user:secret@harnx.example.com",
            "https://harnx.example.com/?x=1",
            "https://harnx.example.com/#top",
        ] {
            assert!(configured(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn session_urls_match_the_web_ui_routes() {
        let key = SessionKey::local("pantheon/atlas", "a-b_c");
        assert_eq!(
            session_url("https://harnx.example.com", &key, None),
            "https://harnx.example.com/agents/pantheon%2Fatlas/sessions/a-b_c"
        );
        let remote = SessionKey::new(
            crate::session_actor::ResolvedAgentTarget::new("metis", "prod"),
            "s1",
        );
        assert_eq!(
            session_url("https://h.example", &remote, None),
            "https://h.example/agents/metis%40prod/sessions/s1"
        );
        assert_eq!(
            session_url("https://h.example", &remote, Some("prod")),
            "https://h.example/agents/metis/sessions/s1"
        );
    }
}
