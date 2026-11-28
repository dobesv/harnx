//! OAuth 2.0 client credentials (RFC 6749 §4.4), with a lazy in-memory cache.

use std::{borrow::Cow, fmt, path::Path, sync::Arc};

use anyhow::{anyhow, Result};
use reqwest::header::HeaderValue;
use url::{form_urlencoded, Url};

use crate::redact::{redact_secret_variants, redact_secrets};

mod cache;
mod exchange;
mod token;

use cache::{Core, Rejection, Why};
use exchange::Grant;

/// Credential snapshot for one request. Keep it until the response arrives:
/// two exchanges can return identical bearer bytes but install distinct
/// generations. The snapshot must never be serialized or logged as text.
#[derive(Clone)]
pub struct AccessToken(Arc<HeaderValue>);

impl AccessToken {
    /// Return a sensitive `Authorization` header value (`Bearer …`).
    pub fn authorization_header(&self) -> HeaderValue {
        self.0.as_ref().clone()
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken([redacted])")
    }
}

/// One service account's token exchange and shared cache. Clones share the
/// same credential generations and in-flight exchange.
#[derive(Clone)]
pub struct ClientCredentialsProvider {
    core: Arc<Core>,
}

impl ClientCredentialsProvider {
    /// Read the file-only client secret (stripping only trailing CR/LF). The
    /// first token exchange is lazy. Call this on a blocking thread.
    ///
    /// The caller must validate `endpoint` and supply a dedicated client with
    /// redirects and ambient proxies disabled, no API credentials, and the
    /// caller's intended TLS and destination policy. This constructor does no
    /// network I/O and does not validate that policy.
    pub fn new(
        endpoint: Url,
        client_id: String,
        client_secret_file: &Path,
        scopes: Vec<String>,
        http: reqwest::Client,
    ) -> Result<Self> {
        let grant = Grant::new(endpoint, client_id, client_secret_file, scopes, http)?;
        Ok(Self {
            core: Arc::new(Core::new(grant)),
        })
    }

    /// Obtain a token for a new request, refreshing lazily within 60 seconds
    /// of expiry. If a concurrent rejection empties the cache between the
    /// freshness check and the snapshot, try once more before failing closed.
    pub async fn token(&self) -> Result<AccessToken> {
        self.ensure_fresh().await?;
        if let Some(token) = self.core.current_token() {
            return Ok(token);
        }
        self.ensure_fresh().await?;
        self.require_cached_token()
    }

    /// Retire only the exact generation that received a 401. No network I/O
    /// and no replay. Returns whether the current generation was retired.
    pub fn reject(&self, sent: &AccessToken) -> bool {
        self.core.reject(sent)
    }

    /// After a 401, obtain a token for a caller-approved replay, without
    /// replaying anything here. Retire only the sent generation. A replacement
    /// already installed by another caller is reusable until actual expiry.
    // qual:api
    pub async fn renew_rejected(&self, sent: &AccessToken) -> Result<AccessToken> {
        Core::acquire_or_join(&self.core, Why::Rejected(Rejection::Generation(sent))).await?;
        self.require_cached_token()
    }

    fn require_cached_token(&self) -> Result<AccessToken> {
        self.core
            .current_token()
            .ok_or_else(|| anyhow!("client-credentials token is unavailable; retry the call"))
    }

    /// Scrub the client secret and current/recently retired tokens, including
    /// their bearer-header, form-encoded and percent-encoded variants.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let secrets = self.core.token_secrets();
        let mut refs = secrets.iter().map(String::as_str).collect::<Vec<_>>();
        refs.push(self.core.grant.client_secret());
        redact_secret_variants(text, &refs)
    }

    /// Keep MCP's separate freshness-then-stamp boundary. New consumers should
    /// use [`Self::token`] instead.
    #[doc(hidden)]
    pub async fn ensure_fresh(&self) -> Result<()> {
        Core::acquire_or_join(&self.core, Why::Proactive).await
    }

    /// Snapshot the cached header without I/O or a second freshness check.
    /// For MCP's separate stamp step only.
    #[doc(hidden)]
    pub fn cached_header(&self) -> Option<HeaderValue> {
        self.core
            .current_token()
            .map(|token| token.authorization_header())
    }

    /// MCP compatibility: compare the header that rmcp sent by value, with
    /// `None` meaning mandatory refresh. New consumers should keep the
    /// generation-bearing snapshot and use [`Self::reject`] or
    /// [`Self::renew_rejected`].
    #[doc(hidden)]
    pub async fn refresh_after_header_rejection(&self, header: Option<&HeaderValue>) -> Result<()> {
        Core::acquire_or_join(&self.core, Why::Rejected(Rejection::Header(header))).await
    }

    /// Preserve the exact approved MCP redaction behavior. REST consumers use
    /// [`Self::redact`] for encoded variants of tokens as well.
    #[doc(hidden)]
    pub fn redact_mcp<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut secrets = self.core.token_secrets();
        let secret = self.core.grant.client_secret();
        secrets.push(secret.to_owned());
        let encoded = form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>();
        if encoded != secret {
            secrets.push(encoded);
        }
        redact_secrets(text, &secrets)
    }
}
