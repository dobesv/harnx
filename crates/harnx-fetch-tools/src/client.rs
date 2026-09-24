use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Proxy, Url};

use crate::net::{check_url, redirect_policy, GuardedResolver, Lookup};

pub const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "curl/8.6.0";

#[derive(Debug)]
pub struct FetchedPage {
    pub body: String,
    pub final_url: Url,
}

/// Only HTTP entry point used by fetch tools and extractors.
#[derive(Clone)]
pub struct FetchClient {
    client: Client,
    resolver: GuardedResolver,
    allow_private_ip: bool,
}

impl FetchClient {
    pub fn new(allow_private_ip: bool) -> Result<Self, String> {
        Self::with_resolver(GuardedResolver::new(allow_private_ip), allow_private_ip)
    }

    #[doc(hidden)]
    pub fn with_lookup(allow_private_ip: bool, lookup: Arc<dyn Lookup>) -> Result<Self, String> {
        Self::with_resolver(
            GuardedResolver::with_lookup(allow_private_ip, lookup),
            allow_private_ip,
        )
    }

    fn with_resolver(resolver: GuardedResolver, allow_private_ip: bool) -> Result<Self, String> {
        let client = build_client(&resolver, allow_private_ip, None)?;
        Ok(Self {
            client,
            resolver,
            allow_private_ip,
        })
    }

    pub fn allow_private_ip(&self) -> bool {
        self.allow_private_ip
    }

    pub async fn fetch(
        &self,
        raw_url: &str,
        headers: &HashMap<String, String>,
        proxy: Option<&str>,
    ) -> Result<FetchedPage, String> {
        let url = Url::parse(raw_url).map_err(|error| format!("invalid URL: {error}"))?;
        check_url(&url, self.allow_private_ip)?;

        let proxy = proxy.map(str::trim).filter(|value| !value.is_empty());
        if proxy.is_some() && !self.allow_private_ip {
            return Err(
                "proxy is disabled while private-IP protection is active; start with --allow-private-ip to permit it"
                    .to_owned(),
            );
        }
        let request_headers = parse_headers(headers)?;
        let proxied;
        let client = if let Some(proxy_url) = proxy {
            // Proxy-specific clients stay inside this guarded boundary and retain
            // URL checks, redirect limits, TLS, DNS, timeouts, and body limits.
            proxied = build_client(&self.resolver, self.allow_private_ip, Some(proxy_url))?;
            &proxied
        } else {
            &self.client
        };

        tokio::time::timeout(REQUEST_TIMEOUT, fetch_inner(client, url, request_headers))
            .await
            .map_err(|_| "request timed out after 30 seconds".to_owned())?
    }
}

fn build_client(
    resolver: &GuardedResolver,
    allow_private_ip: bool,
    proxy: Option<&str>,
) -> Result<Client, String> {
    let mut builder = Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(redirect_policy(allow_private_ip))
        .dns_resolver(resolver.clone());
    if !allow_private_ip || proxy.is_some() {
        builder = builder.no_proxy();
    }
    if let Some(proxy_url) = proxy {
        let configured =
            Proxy::all(proxy_url).map_err(|error| format!("invalid proxy: {error}"))?;
        builder = builder.proxy(configured);
    }
    builder
        .build()
        .map_err(|error| format!("failed to construct HTTP client: {error}"))
}

fn parse_headers(headers: &HashMap<String, String>) -> Result<HeaderMap, String> {
    let mut parsed = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("invalid header name '{name}': {error}"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|error| format!("invalid value for header '{name}': {error}"))?;
        parsed.insert(name, value);
    }
    Ok(parsed)
}

async fn fetch_inner(client: &Client, url: Url, headers: HeaderMap) -> Result<FetchedPage, String> {
    let response = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|error| format!("request failed: {}", error_chain(&error)))?;
    let final_url = response.url().clone();
    let status = response.status();
    if !status.is_success() {
        return Err(format!("request failed with HTTP status {status}"));
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| format!("failed while reading response body: {error}"))?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(format!(
                "response body exceeds {} byte limit",
                MAX_RESPONSE_BYTES
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(FetchedPage {
        body: String::from_utf8_lossy(&body).into_owned(),
        final_url,
    })
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(error) = source {
        let message = error.to_string();
        if messages.last() != Some(&message) {
            messages.push(message);
        }
        source = error.source();
    }
    messages.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_headers_without_panicking() {
        let headers = HashMap::from([("bad header".to_owned(), "value".to_owned())]);
        assert!(parse_headers(&headers)
            .unwrap_err()
            .contains("invalid header"));
    }
}
