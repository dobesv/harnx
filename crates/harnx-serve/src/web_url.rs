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
use anyhow::Result;
use harnx_runtime::{config::Config, identity::first_value, web_url::normalize_public_url};
use http::{HeaderMap, Uri};

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

/// The address that opens `key`'s session in the Web UI served at `base`,
/// naming the agent as the Web UI does: without the cluster suffix when the
/// agent is on `default_cluster`.
pub(crate) fn session_url(base: &str, key: &SessionKey, default_cluster: Option<&str>) -> String {
    let agent = key
        .target()
        .display_ref_with_default_cluster(default_cluster);
    harnx_runtime::web_url::session_url(base, &agent, key.session())
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
