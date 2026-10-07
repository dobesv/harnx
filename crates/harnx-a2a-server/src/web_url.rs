//! Agent Card base URLs. Inference matches harnx-serve's `web_url` module.
//!
//! Forwarded headers are trusted hints. A proxy must overwrite client-supplied
//! values, or deployments should set `--public-base-url` instead.

use anyhow::{ensure, Context, Result};
use axum::http::{uri::Authority, HeaderMap, Uri};

pub(crate) fn normalize_public_base_url(raw: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let url = reqwest::Url::parse(raw)
        .with_context(|| format!("public-base-url '{raw}' is not a URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "public-base-url '{raw}' must be an http or https URL without credentials, query or fragment"
    );
    Ok(Some(url.as_str().trim_end_matches('/').to_string()))
}

// Keep this small copy in sync with harnx-serve/src/web_url.rs::inferred_base_url.
// Depending on the whole web frontend just for URL inference would be excessive.
pub(crate) fn inferred_base_url(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let host = first_value(headers, "x-forwarded-host")
        .or_else(|| first_value(headers, "host"))
        .or_else(|| uri.authority().map(|authority| authority.as_str()))?;
    let authority = host.parse::<Authority>().ok()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let scheme = match first_value(headers, "x-forwarded-proto") {
        Some(proto) if proto.eq_ignore_ascii_case("https") => "https",
        _ => "http",
    };
    Some(format!("{scheme}://{authority}"))
}

pub(crate) fn first_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_base_url_is_checked_and_normalized() {
        harnx_core::require_nextest();
        for (raw, expected) in [
            ("https://Harnx.Example.com/", "https://harnx.example.com"),
            (" https://example.com/harnx/ ", "https://example.com/harnx"),
        ] {
            assert_eq!(
                normalize_public_base_url(Some(raw)).unwrap().as_deref(),
                Some(expected)
            );
        }
        assert_eq!(normalize_public_base_url(None).unwrap(), None);
        assert_eq!(normalize_public_base_url(Some(" ")).unwrap(), None);
        for invalid in [
            "harnx.example.com",
            "ftp://harnx.example.com",
            "https://user:secret@harnx.example.com",
            "https://harnx.example.com/?x=1",
            "https://harnx.example.com/#top",
        ] {
            assert!(
                normalize_public_base_url(Some(invalid)).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn inference_handles_authority_fallback_and_invalid_hosts() {
        harnx_core::require_nextest();
        assert_eq!(
            inferred_base_url(&HeaderMap::new(), &Uri::from_static("/")),
            None
        );
        assert_eq!(
            inferred_base_url(
                &HeaderMap::new(),
                &Uri::from_static("http://example.com:8443/agents")
            ),
            Some("http://example.com:8443".into())
        );
        for host in ["user@example.com", "example.com/agents"] {
            let mut headers = HeaderMap::new();
            headers.insert("host", host.parse().unwrap());
            assert_eq!(inferred_base_url(&headers, &Uri::from_static("/")), None);
        }
    }
}
