//! Connect pages and completing routes (decision 7).
//!
//! Every route loads the encrypted pending flow from the shared store, so any
//! instance of the tool server can serve any step, including after the
//! instance that created the link stops. Nothing here writes before the
//! `pending -> claimed` CAS succeeds, and a request whose trusted user header
//! doesn't match the flow owner is refused with no writes.
//!
//! Request URLs, query strings and form bodies never reach a log statement.
//! OAuth routes delegate to [`OAuthFlowBackend`]; token-only servers need no
//! provider backend.

mod completion;
mod gate;
mod oauth;
mod pages;
#[cfg(test)]
mod tests;

use anyhow::Result;
use async_trait::async_trait;
use axum::routing::{get, post};
use axum::Router;
use harnx_credentials::{NewCredential, PendingFlow, Versioned};
use std::sync::Arc;
use url::Url;

use crate::native::registrations::Lifecycle;
use completion::token_complete;
use oauth::{callback, oauth_start};
use pages::connect_page;

/// T6's OAuth implementation: build the provider redirect for one pending
/// flow, and turn an authorization code into stored credential material.
/// Both run after the shared route checks. Code exchange also requires a claim CAS.
#[async_trait]
pub trait OAuthFlowBackend: Send + Sync {
    /// The provider authorization URL for one pending flow. The flow already
    /// carries `oauth_state`, PKCE verifier, scopes and client registration.
    async fn redirect_url(&self, flow: &Versioned<PendingFlow>) -> Result<Url, String>;

    /// Exchange one authorization code for stored credential material. The
    /// outcome's `expires_at` is `None` (the server TTL applies).
    async fn exchange_code(
        &self,
        flow: &Versioned<PendingFlow>,
        code: &str,
    ) -> Result<NewCredential, String>;

    async fn exchange_code_with_issuer(
        &self,
        flow: &Versioned<PendingFlow>,
        code: &str,
        _issuer: Option<&str>,
    ) -> Result<NewCredential, String> {
        self.exchange_code(flow, code).await
    }
}

#[derive(Clone)]
struct AppState {
    lifecycle: Arc<Lifecycle>,
    /// Absent on token-only servers.
    oauth: Option<Arc<dyn OAuthFlowBackend>>,
}

/// Router for connect pages under the public base URL's path.
pub fn router(lifecycle: Arc<Lifecycle>, base_url: &str) -> Router {
    router_with_oauth(
        lifecycle.clone(),
        lifecycle
            .oauth
            .clone()
            .map(|backend| backend as Arc<dyn OAuthFlowBackend>),
        base_url,
    )
}

/// Like [`router`], with an explicit OAuth backend.
pub fn router_with_oauth(
    lifecycle: Arc<Lifecycle>,
    oauth: Option<Arc<dyn OAuthFlowBackend>>,
    base_url: &str,
) -> Router {
    let inner = Router::new()
        .route("/connect/{id}", get(connect_page))
        .route("/connect/{id}/oauth", get(oauth_start))
        .route("/connect/{id}/token", post(token_complete))
        .route("/callback", get(callback))
        .with_state(AppState { lifecycle, oauth });
    let prefix = base_url
        .parse::<Url>()
        .ok()
        .map(|url| url.path().trim_end_matches('/').to_string())
        .unwrap_or_default();
    if prefix.is_empty() {
        inner
    } else {
        Router::new().nest(&prefix, inner)
    }
}
