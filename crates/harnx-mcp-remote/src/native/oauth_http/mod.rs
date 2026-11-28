//! Outbound HTTP transport for OAuth metadata discovery, DCR, and token exchange.
//!
//! Enforces SSRF defense, prevents redirect following, bounds response bodies
//! to 1 MiB, bounds request timeouts, redacts errors statically, sanitizes
//! non-success token responses to standard OAuth error codes, and captures full
//! DCR `ClientRegistrationResponse` payloads for durable flow storage.

mod policy;
mod transport;

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Context as _};
use harnx_credentials::Secret;
use rmcp::transport::auth::{
    AuthorizationMetadata, ClientRegistrationResponse, OAuthHttpClient, OAuthHttpClientError,
    OAuthHttpClientFuture, OAuthHttpRequest,
};
use tracing::instrument::WithSubscriber;
use url::Url;

use super::NativeConfig;
use harnx_http_auth::net::GuardedResolver;
pub(crate) use policy::AddressClass;
#[cfg(test)]
pub(crate) use transport::sanitize_oauth_error_body;

/// Default and maximum upper bound for OAuth HTTP request timeouts.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

const USERINFO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_USERINFO_BODY_BYTES: usize = 64 * 1024;

pub use harnx_http_auth::oauth::MAX_RESPONSE_BODY_BYTES;
use harnx_http_auth::oauth::STANDARD_OAUTH_ERRORS;

/// Static error type that never formats dynamic strings, URLs, or secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StaticError(pub &'static str);

pub(crate) fn static_error(msg: &'static str) -> OAuthHttpClientError {
    Box::new(StaticError(msg))
}

/// Outbound HTTP client for OAuth operations (metadata, registration, token exchange).
#[derive(Clone)]
pub(crate) struct OAuthHttp {
    client: reqwest::Client,
    upstream_scheme: String,
    upstream_host: String,
    upstream_port: Option<u16>,
    upstream_is_loopback: bool,
    allowed_classes: HashSet<AddressClass>,
    dcr_registration: Arc<Mutex<Option<Secret<ClientRegistrationResponse>>>>,
}

impl OAuthHttp {
    /// Constructs a new `OAuthHttp` client from server configuration using shared TLS.
    pub fn new(config: &NativeConfig) -> anyhow::Result<Self> {
        let upstream_url = Url::parse(&config.url)
            .with_context(|| format!("invalid upstream URL '{}'", config.url))?;
        let upstream_scheme = upstream_url.scheme().to_ascii_lowercase();
        let upstream_host = upstream_url.host_str().unwrap_or("").to_ascii_lowercase();
        let upstream_port = upstream_url.port_or_known_default();
        let upstream_is_loopback = is_loopback_host(&upstream_url);
        let allowed_classes = policy::determine_allowed_classes(&upstream_url);

        let builder = super::pool::http_client_builder(config)?;
        let resolver = Arc::new(GuardedResolver::new(allowed_classes.clone()));
        let client = builder
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(resolver)
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .context("build guarded OAuth HTTP client")?;

        Ok(Self {
            client,
            upstream_scheme,
            upstream_host,
            upstream_port,
            upstream_is_loopback,
            allowed_classes,
            dcr_registration: Arc::new(Mutex::new(None)),
        })
    }

    /// Exposes and clears any captured DCR `ClientRegistrationResponse`.
    pub fn take_registration(&self) -> Option<Secret<ClientRegistrationResponse>> {
        self.dcr_registration
            .lock()
            .map(|mut lock| lock.take())
            .unwrap_or(None)
    }

    /// Validates whether a target URL is permitted by scheme, credential/fragment, and address policy.
    pub fn is_url_allowed(&self, url: &Url) -> bool {
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return false;
        }
        if !self.is_scheme_allowed(url) {
            return false;
        }
        self.is_host_allowed(url)
    }

    fn is_scheme_allowed(&self, url: &Url) -> bool {
        let scheme = url.scheme().to_ascii_lowercase();
        if scheme == "http" {
            if self.upstream_scheme != "http" || !self.upstream_is_loopback {
                return false;
            }
            let host = url.host_str().unwrap_or("").to_ascii_lowercase();
            host == self.upstream_host && url.port_or_known_default() == self.upstream_port
        } else {
            scheme == "https"
        }
    }

    fn is_host_allowed(&self, url: &Url) -> bool {
        match policy::literal_ip(url) {
            Some(ip) => {
                let class = AddressClass::classify(ip);
                class != AddressClass::Disallowed && self.allowed_classes.contains(&class)
            }
            None => {
                let host = url.host_str().unwrap_or("");
                !policy::is_cloud_metadata_host(host)
            }
        }
    }

    /// Pre-validates a discovered authorization URL (including DNS resolution) before browser redirect.
    pub(crate) async fn validate_url(&self, url: &Url) -> anyhow::Result<()> {
        if !self.is_url_allowed(url) {
            bail!("authorization URL disallowed by scheme or address policy");
        }
        policy::prevalidate_endpoint_dns(url, &self.allowed_classes).await
    }

    /// Optional userinfo helper to extract an account display label.
    pub(crate) async fn account_label(
        &self,
        metadata: &AuthorizationMetadata,
        access_token: &str,
    ) -> Option<String> {
        let endpoint_str = metadata
            .additional_fields
            .get("userinfo_endpoint")?
            .as_str()?;
        let url = Url::parse(endpoint_str).ok()?;
        if !self.is_url_allowed(&url) {
            return None;
        }

        let mut bearer =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {access_token}")).ok()?;
        bearer.set_sensitive(true);

        let req = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, bearer)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(USERINFO_TIMEOUT)
            .build()
            .ok()?;

        let mut response = self
            .client
            .execute(req)
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await
            .ok()?;

        if !response.status().is_success() {
            return None;
        }

        let body = transport::read_bounded_body(&mut response, MAX_USERINFO_BODY_BYTES)
            .await
            .ok()?;
        transport::extract_account_label(&body)
    }

    async fn execute_inner(
        &self,
        request: OAuthHttpRequest,
    ) -> Result<oauth2::HttpResponse, OAuthHttpClientError> {
        let OAuthHttpRequest {
            request, timeout, ..
        } = request;

        let uri_str = request.uri().to_string();
        let url = Url::parse(&uri_str).map_err(|_| static_error("invalid oauth request uri"))?;
        if !self.is_url_allowed(&url) {
            return Err(static_error("oauth request address or scheme disallowed"));
        }

        let is_post = request.method() == oauth2::http::Method::POST;
        let req = transport::build_request(&self.client, request, url, timeout)?;

        let mut response = self
            .client
            .execute(req)
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await
            .map_err(|_| static_error("oauth http request failed"))?;

        if response.status().is_redirection() {
            return Err(static_error("OAuth redirects are not allowed"));
        }

        let body = transport::read_response_body(&mut response, MAX_RESPONSE_BODY_BYTES).await?;

        if response.status().is_success() {
            if is_post {
                transport::try_capture_dcr(&body, &self.dcr_registration);
            }
            transport::build_success_response(response, body)
        } else {
            transport::build_error_response(response, &body)
        }
    }
}

impl OAuthHttpClient for OAuthHttp {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move { self.execute_inner(request).await })
    }
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => {
            let lower = domain.to_ascii_lowercase();
            lower == "localhost" || lower.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(v4)) => {
            AddressClass::classify(std::net::IpAddr::V4(v4)) == AddressClass::Loopback
        }
        Some(url::Host::Ipv6(v6)) => {
            AddressClass::classify(std::net::IpAddr::V6(v6)) == AddressClass::Loopback
        }
        None => false,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod redirect_tests;
