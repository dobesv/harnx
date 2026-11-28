//! MCP adapter for the shared service-account token provider.

use std::{borrow::Cow, path::Path};

use anyhow::Result;
use async_trait::async_trait;
use harnx_http_auth::ClientCredentialsProvider as TokenProvider;
use reqwest::header::{HeaderValue, AUTHORIZATION};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use url::Url;

use crate::UpstreamAuth;

#[cfg(test)]
mod tests;

/// Service-account credentials for rmcp. Holds no independent cache.
pub struct ClientCredentialsProvider(TokenProvider);

impl ClientCredentialsProvider {
    /// Load the client secret now; defer the first exchange until a request.
    pub fn new(
        endpoint: Url,
        client_id: String,
        client_secret_file: &Path,
        scopes: Vec<String>,
        http: reqwest::Client,
    ) -> Result<Self> {
        TokenProvider::new(endpoint, client_id, client_secret_file, scopes, http).map(Self)
    }
}

#[async_trait]
impl UpstreamAuth for ClientCredentialsProvider {
    fn apply(&self, config: &mut StreamableHttpClientTransportConfig) {
        if let Some(header) = self.0.cached_header() {
            config.custom_headers.insert(AUTHORIZATION, header);
        } else {
            config.custom_headers.remove(&AUTHORIZATION);
        }
    }

    fn apply_for_request(&self, config: &mut StreamableHttpClientTransportConfig) -> Result<()> {
        self.apply(config);
        // A concurrent rejection can invalidate the cache after refresh
        // returned. Fail closed rather than send without a usable token.
        anyhow::ensure!(
            config.custom_headers.contains_key(&AUTHORIZATION),
            "client-credentials token is unavailable; retry the call"
        );
        Ok(())
    }

    async fn ensure_fresh(&self) -> Result<()> {
        self.0.ensure_fresh().await
    }

    fn refreshable(&self) -> bool {
        true
    }

    async fn refresh_after_rejection(&self, rejected: Option<&HeaderValue>) -> Result<()> {
        self.0.refresh_after_header_rejection(rejected).await
    }

    fn reconnect_message(&self) -> String {
        "client-credentials token refresh failed; check the OAuth client configuration and token endpoint"
            .into()
    }

    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        self.0.redact_mcp(text)
    }
}
