//! How one credential's secret material reaches the upstream transport.
//!
//! The token implementation lives here; OAuth plugs in through the same
//! factory by building an rmcp `AuthorizationManager` from the stored
//! registration and the `harnx-credentials` refresh adapters. The pool
//! re-exports these traits because every consumer reaches them through
//! `native::pool`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use harnx_credentials::{CredentialMaterial, CredentialRecord, Versioned};
use reqwest::header::{HeaderName, HeaderValue};
use std::borrow::Cow;
use std::sync::Arc;

use crate::secret_redaction::redact_secrets;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

/// How one credential's secret material reaches the upstream transport, and
/// how its echo is redacted from upstream-sourced text.
#[async_trait]
pub trait UpstreamAuth: Send + Sync {
    /// Stamp cached credentials onto one transport config without I/O.
    fn apply(&self, config: &mut StreamableHttpClientTransportConfig);

    /// Stamp credentials for a send without I/O. If a refreshable provider
    /// loses its cached credential, a failed initial stamp gets one more
    /// freshness check and stamp. A failed replay stamp is final.
    fn apply_for_request(&self, config: &mut StreamableHttpClientTransportConfig) -> Result<()> {
        self.apply(config);
        Ok(())
    }

    /// Refresh before use when the access token is near expiry; token
    /// material is a no-op.
    async fn ensure_fresh(&self) -> Result<()> {
        Ok(())
    }

    /// Whether an upstream 401 should force one refresh and a single retry.
    /// Token material is never refreshable.
    fn refreshable(&self) -> bool {
        false
    }

    /// Refresh after a 401 unless another caller has installed a usable
    /// replacement. On success the caller uses [`Self::apply_for_request`]
    /// without another [`Self::ensure_fresh`]. If the credential has since
    /// been invalidated, stamping can fail and prevent the replay.
    async fn refresh_after_rejection(&self, _rejected: Option<&HeaderValue>) -> Result<()> {
        anyhow::bail!("upstream credential rejected")
    }

    fn reconnect_message(&self) -> String {
        "upstream credential rejected; reconnect this server".into()
    }

    /// Redact this credential's secret material from upstream-sourced text.
    /// A provider can echo the submitted credential back — in a JSON-RPC
    /// error, in tool result content, in a handshake failure — and that text
    /// must never reach a tool result, page or the journal as-is.
    /// Implementations list every form the secret can appear in (raw token,
    /// `scheme token`, `Bearer <token>`).
    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }
}

/// Builds the [`UpstreamAuth`] for one stored credential.
pub trait UpstreamAuthFactory: Send + Sync {
    fn build(&self, record: &Versioned<CredentialRecord>) -> Result<Arc<dyn UpstreamAuth>>;
}

/// Token/API-key credentials: one configured header, optional scheme.
pub struct TokenAuthFactory {
    pub header: HeaderName,
    pub scheme: Option<String>,
}

pub struct TokenUpstreamAuth {
    header: HeaderName,
    value: HeaderValue,
    /// Every form this credential's secret can appear in upstream-sourced
    /// text: the raw token and the full header value when a scheme prefixes
    /// it.
    secrets: Vec<String>,
}

impl TokenAuthFactory {
    fn header_value(material: &CredentialMaterial) -> Result<HeaderValue> {
        let harnx_credentials::CredentialMaterial::Token(token) = material else {
            anyhow::bail!("credential is not a token; OAuth material needs the OAuth auth factory");
        };
        let raw = match &token.scheme {
            Some(scheme) => format!("{scheme} {}", token.token.expose()),
            None => token.token.expose().clone(),
        };
        HeaderValue::from_str(&raw).context("token is not a valid header value")
    }

    fn secret_forms(material: &CredentialMaterial) -> Vec<String> {
        let harnx_credentials::CredentialMaterial::Token(token) = material else {
            return Vec::new();
        };
        let mut secrets = vec![token.token.expose().clone()];
        if let Some(scheme) = &token.scheme {
            secrets.push(format!("{scheme} {}", token.token.expose()));
        }
        secrets
    }
}

impl UpstreamAuthFactory for TokenAuthFactory {
    fn build(&self, record: &Versioned<CredentialRecord>) -> Result<Arc<dyn UpstreamAuth>> {
        let mut value = Self::header_value(&record.record.material)?;
        value.set_sensitive(true);
        Ok(Arc::new(TokenUpstreamAuth {
            header: self.header.clone(),
            value,
            secrets: Self::secret_forms(&record.record.material),
        }))
    }
}

impl UpstreamAuth for TokenUpstreamAuth {
    fn apply(&self, config: &mut StreamableHttpClientTransportConfig) {
        config
            .custom_headers
            .insert(self.header.clone(), self.value.clone());
    }

    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        redact_secrets(text, &self.secrets)
    }
}
