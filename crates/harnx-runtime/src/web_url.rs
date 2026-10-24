//! Shared addresses of sessions in the Web UI.
//!
//! A browser opens a session at `{base}/agents/{agent}/sessions/{session}`.
//! Agent and session components use the same encoding as the Web UI.

use crate::config::LOCAL_CLUSTER_KEY;
use anyhow::{ensure, Context, Result};
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

/// Check the configured `serve_public_url` and remove trailing slashes.
/// It may carry a path when the Web UI is served under a prefix.
/// Missing or blank values mean no public URL is configured.
pub fn normalize_public_url(raw: Option<&str>) -> Result<Option<String>> {
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

/// Display an agent without its cluster suffix when local or on the default cluster.
pub fn agent_display_ref(name: &str, cluster: &str, default_cluster: Option<&str>) -> String {
    if cluster == LOCAL_CLUSTER_KEY || Some(cluster) == default_cluster {
        name.to_string()
    } else {
        format!("{name}@{cluster}")
    }
}

/// Open a session in the Web UI at a normalized `base` URL.
/// `agent_display` is the display reference, not yet percent-encoded.
pub fn session_url(base: &str, agent_display: &str, session_id: &str) -> String {
    format!(
        "{base}/agents/{}/sessions/{}",
        utf8_percent_encode(agent_display, URI_COMPONENT),
        utf8_percent_encode(session_id, URI_COMPONENT)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_public_url_is_checked_and_normalized() {
        assert_eq!(normalize_public_url(None).unwrap(), None);
        assert_eq!(normalize_public_url(Some("  ")).unwrap(), None);
        assert_eq!(
            normalize_public_url(Some("https://Harnx.Example.com/"))
                .unwrap()
                .as_deref(),
            Some("https://harnx.example.com")
        );
        assert_eq!(
            normalize_public_url(Some("https://example.com/harnx/"))
                .unwrap()
                .as_deref(),
            Some("https://example.com/harnx")
        );
        for invalid in [
            "harnx.example.com",
            "ftp://harnx.example.com",
            "https://user:secret@harnx.example.com",
            "https://harnx.example.com/?x=1",
            "https://harnx.example.com/#top",
        ] {
            assert!(normalize_public_url(Some(invalid)).is_err(), "{invalid}");
        }
    }

    #[test]
    fn agent_display_ref_omits_local_and_default_clusters() {
        for default in [None, Some("prod"), Some(LOCAL_CLUSTER_KEY)] {
            assert_eq!(
                agent_display_ref("metis", LOCAL_CLUSTER_KEY, default),
                "metis"
            );
        }
        assert_eq!(agent_display_ref("metis", "prod", Some("prod")), "metis");
        assert_eq!(agent_display_ref("metis", "prod", None), "metis@prod");
        assert_eq!(
            agent_display_ref("metis", "prod", Some("other")),
            "metis@prod"
        );
    }

    #[test]
    fn session_urls_match_the_web_ui_routes() {
        assert_eq!(
            session_url("https://harnx.example.com", "pantheon/atlas", "a-b_c"),
            "https://harnx.example.com/agents/pantheon%2Fatlas/sessions/a-b_c"
        );
        assert_eq!(
            session_url("https://h.example", "metis@prod", "s1"),
            "https://h.example/agents/metis%40prod/sessions/s1"
        );
        assert_eq!(
            session_url("https://h.example", "metis", "s1"),
            "https://h.example/agents/metis/sessions/s1"
        );
    }

    #[test]
    fn session_components_match_encode_uri_component() {
        assert_eq!(
            session_url("https://h.example/prefix", "a /@%?#é", "s /@%?#雪"),
            "https://h.example/prefix/agents/a%20%2F%40%25%3F%23%C3%A9/sessions/s%20%2F%40%25%3F%23%E9%9B%AA"
        );
        assert_eq!(
            session_url("https://h.example", "AZaz09-_.!~*'()", "-_.!~*'()"),
            "https://h.example/agents/AZaz09-_.!~*'()/sessions/-_.!~*'()"
        );
    }
}
