//! GET-only network capability, scoped to one middleware entry, not the tool allowlist.

use super::frame::Frame;
use anyhow::{anyhow, bail, ensure, Context, Result};
use harnx_script::ScriptHostContext;
use reqwest::{redirect::Policy, Client, Url};
use serde_json::{json, Value};
use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY_BYTES: usize = 256 * 1024;

static CLIENT: LazyLock<Result<Client, String>> = LazyLock::new(|| {
    Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(super::fetch_net::Resolver))
        .redirect(Policy::none())
        .referer(false)
        .retry(reqwest::retry::never())
        // Keep the byte limit independent of workspace feature unification.
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())
});

impl Frame {
    pub async fn fetch(self: &Arc<Self>, text: &str, host: &ScriptHostContext) -> Result<Value> {
        let _operation = self.begin()?;
        fetch_url(text, &self.fetch_hosts, &host.cancel).await
    }
}

/// GET one allowlisted URL. Shared by tool middleware and event hooks: both
/// gate on the entry's host allowlist and block metadata/link-local addresses.
pub(crate) async fn fetch_url(
    text: &str,
    hosts: &[String],
    cancel: &harnx_script::CancellationToken,
) -> Result<Value> {
    ensure!(
        !hosts.is_empty(),
        "fetch denied: no fetch host allowlist configured"
    );
    let url = checked_url(text, hosts)?;
    tokio::select! {
        () = cancel.cancelled() => bail!("fetch cancelled"),
        result = tokio::time::timeout(FETCH_TIMEOUT, get(url)) => {
            result.map_err(|_| anyhow!("fetch timed out after 10s"))?
        }
    }
}

fn checked_url(text: &str, hosts: &[String]) -> Result<Url> {
    let url = Url::parse(text).context("fetch requires an absolute HTTP(S) URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "fetch only supports HTTP(S)"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "fetch URL credentials are not allowed"
    );
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("fetch URL has no host"))?;
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .ok();
    // URL and std format IPv4-mapped IPv6 differently; use the config's form.
    let normalized = match ip {
        Some(std::net::IpAddr::V6(ip)) => format!("[{ip}]"),
        _ => host.to_owned(),
    };
    ensure!(
        hosts.contains(&normalized),
        "fetch denied: host '{host}' is not in this middleware's fetch allowlist"
    );
    if let Some(ip) = ip {
        super::fetch_net::check_ip(ip)?;
    }
    Ok(url)
}

async fn get(url: Url) -> Result<Value> {
    let client = CLIENT
        .as_ref()
        .map_err(|error| anyhow!("fetch client: {error}"))?;
    let mut response = client.get(url).send().await.map_err(request_error)?;
    let status = response.status().as_u16();
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_BODY_BYTES as u64),
        "fetch response exceeds 262144-byte limit"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        ensure!(
            chunk.len() <= MAX_BODY_BYTES - body.len(),
            "fetch response exceeds 262144-byte limit"
        );
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body).context("fetch response body is not UTF-8")?;
    Ok(json!({"status": status, "body": body}))
}

fn request_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow!("fetch timed out after 10s")
    } else {
        // URLs may contain query secrets. Never include them in runtime failures.
        anyhow!("fetch request failed: {}", error.without_url())
    }
}
