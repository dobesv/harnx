//! OAuth 2.0 client-credentials token provider for upstream auth
//! (RFC 6749 §4.4).
//!
//! The provider owns a lazy token cache: the first authenticated request
//! acquires a token, later requests reuse it, and it refreshes inside a
//! fixed 60-second pre-expiry window measured on the monotonic clock.
//! Concurrent callers share a single exchange attempt — its success *and*
//! its safe error — so a slow token endpoint is never stamped with one
//! request per caller. Diagnostics are built from static text, the HTTP
//! status, and at most one allowlisted standard OAuth error code; provider
//! bodies, URLs and credentials never reach them.

use std::{
    borrow::Cow,
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use futures::FutureExt as _;
use harnx_credentials::Secret;
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::{
    sync::Notify,
    time::{timeout_at, Instant},
};
use tracing::instrument::WithSubscriber;
use url::{form_urlencoded, Url};

use crate::native::oauth::read_client_secret_file;
use crate::native::oauth_http::MAX_RESPONSE_BODY_BYTES;
use crate::{secret_redaction::redact_secrets, UpstreamAuth};

#[cfg(test)]
mod tests;
mod token;

use token::{form_body, is_fresh, parse_token_response, status_error, CachedToken};

/// Total budget for one token exchange (native precedent:
/// `oauth_http::DEFAULT_TIMEOUT`). Token-only: it never applies to MCP
/// streams.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// Retired tokens that stay redactable after rotation. Upstream text can
/// echo a token that was in flight when a 401 landed, so a few recent
/// generations must remain redactable — but never an unbounded history.
const RETIRED_SECRET_GENERATIONS: usize = 4;

/// Client-credentials auth for one token endpoint, usable as an
/// [`UpstreamAuth`] behind the shared HTTP adapter.
pub struct ClientCredentialsProvider {
    core: Arc<Core>,
}

impl ClientCredentialsProvider {
    /// Build a provider for one token endpoint. The client secret is read
    /// from `client_secret_file` immediately (trailing CR/LF stripped,
    /// native semantics); the first token exchange happens lazily on the
    /// first authenticated request.
    pub fn new(
        endpoint: Url,
        client_id: String,
        client_secret_file: &Path,
        scopes: Vec<String>,
        http: reqwest::Client,
    ) -> Result<Self> {
        let client_secret = read_client_secret_file(client_secret_file)?;
        Ok(Self {
            core: Arc::new(Core {
                endpoint,
                client_id,
                client_secret,
                scopes,
                http,
                state: Mutex::new(RefreshState::default()),
            }),
        })
    }
}

// qual:allow(srp, god_struct) reason: "The detached exchange runner owns both the grant configuration and the refresh state it publishes into."
struct Core {
    endpoint: Url,
    client_id: String,
    client_secret: Secret<String>,
    scopes: Vec<String>,
    http: reqwest::Client,
    state: Mutex<RefreshState>,
}

/// Cache validity and attempt admission change together under one short lock.
/// A rejected credential moves to redaction-only history before callers join
/// or start the exchange, so even a failed exchange can't make it usable again.
#[derive(Default)]
struct RefreshState {
    cached: Option<CachedToken>,
    retired: VecDeque<Vec<String>>,
    inflight: Option<Arc<Attempt>>,
}

impl RefreshState {
    fn retire_cached(&mut self) {
        if let Some(previous) = self.cached.take() {
            self.retired.push_front(previous.secrets);
            self.retired.truncate(RETIRED_SECRET_GENERATIONS);
        }
    }

    /// `None` means the caller can use the cache without joining any attempt.
    /// A joinable attempt always represents a real exchange, never a cache hit.
    // qual:allow(iosp) reason: "Freshness, rejection invalidation and attempt admission must run under the same state lock."
    fn admit(&mut self, why: Why<'_>) -> Option<(Arc<Attempt>, bool)> {
        match why {
            Why::Proactive => {
                if self
                    .cached
                    .as_ref()
                    .is_some_and(|token| is_fresh(token, Instant::now()))
                {
                    return None;
                }
            }
            Why::Rejected(rejected) => {
                // A late 401 can reuse a replacement until its actual expiry.
                // The 60-second pre-expiry window is only for proactive refresh.
                if self.cached.as_ref().is_some_and(|token| {
                    rejected.is_some_and(|header| header.as_bytes() != token.header.as_bytes())
                        && token
                            .expires_at
                            .is_none_or(|expiry| expiry > Instant::now())
                }) {
                    return None;
                }
                // No header to compare is still a mandatory refresh.
                self.retire_cached();
            }
        }
        if let Some(running) = &self.inflight {
            return Some((Arc::clone(running), false));
        }
        let attempt = Arc::new(Attempt::default());
        self.inflight = Some(Arc::clone(&attempt));
        Some((attempt, true))
    }
}

/// One shared acquisition attempt. The leader spawns a detached runner
/// that performs the exchange and completes the outcome; callers that
/// arrive while it runs — and the leader itself — wait on `done` for the
/// shared result.
///
/// The shared failure is the attempt's rendered error message. Every
/// provider error is a bounded, redacted, static-text diagnostic, so
/// passing the message to the waiting batch is exactly the shared safe
/// error the contract requires.
#[derive(Default)]
struct Attempt {
    done: Notify,
    outcome: Mutex<Option<AttemptOutcome>>,
}

#[derive(Clone)]
enum AttemptOutcome {
    Ok,
    Failed(String),
}

impl AttemptOutcome {
    fn from_result(outcome: Result<()>) -> Self {
        match outcome {
            Ok(()) => AttemptOutcome::Ok,
            Err(error) => AttemptOutcome::Failed(error.to_string()),
        }
    }

    fn into_result(self) -> Result<()> {
        match self {
            AttemptOutcome::Ok => Ok(()),
            AttemptOutcome::Failed(message) => bail!("{message}"),
        }
    }
}

enum Why<'a> {
    Proactive,
    Rejected(Option<&'a HeaderValue>),
}

impl Core {
    fn current_header(&self) -> Option<HeaderValue> {
        lock(&self.state)
            .cached
            .as_ref()
            .map(|token| token.header.clone())
    }

    /// Admission has no await points. The first caller starts the detached
    /// runner before waiting, so cancelling that caller can't strand the slot.
    // qual:allow(iosp) reason: "Start a claimed exchange before the first await, but wait outside the admission lock."
    async fn acquire_or_join(core: &Arc<Self>, why: Why<'_>) -> Result<()> {
        let admitted = lock(&core.state).admit(why);
        let Some((attempt, lead)) = admitted else {
            return Ok(());
        };
        if lead {
            Self::run_attempt(Arc::clone(core), Arc::clone(&attempt));
        }
        Self::await_attempt(&attempt).await
    }

    /// Detached exchange runner. Caller cancellation must not abort an
    /// exchange between the provider consuming the client credentials and
    /// the cache publish, and waiters must always receive an outcome, so
    /// the runner owns completion. A panic is converted to a safe shared
    /// error before it is re-raised.
    fn run_attempt(core: Arc<Self>, attempt: Arc<Attempt>) {
        tokio::spawn(async move {
            let outcome = match std::panic::AssertUnwindSafe(core.exchange())
                .catch_unwind()
                .await
            {
                Ok(outcome) => outcome,
                Err(panic) => {
                    core.finish_attempt(
                        &attempt,
                        Err(anyhow!("token exchange failed; retry the call")),
                    );
                    std::panic::resume_unwind(panic);
                }
            };
            core.finish_attempt(&attempt, outcome);
        });
    }

    async fn await_attempt(attempt: &Arc<Attempt>) -> Result<()> {
        loop {
            if let Some(outcome) = lock(&attempt.outcome).clone() {
                return outcome.into_result();
            }
            let notified = attempt.done.notified();
            if let Some(outcome) = lock(&attempt.outcome).clone() {
                return outcome.into_result();
            }
            notified.await;
        }
    }

    /// The runner publishes the cache, outcome and empty slot together.
    /// Already-admitted callers keep the outcome even if a later call retries.
    fn finish_attempt(&self, attempt: &Attempt, outcome: Result<CachedToken>) {
        {
            let mut state = lock(&self.state);
            let outcome = outcome.map(|token| {
                state.retire_cached();
                state.cached = Some(token);
            });
            *lock(&attempt.outcome) = Some(AttemptOutcome::from_result(outcome));
            state.inflight = None;
        }
        attempt.done.notify_waiters();
    }

    async fn exchange(&self) -> Result<CachedToken> {
        // One absolute token-only deadline covers headers and body. Starting a
        // fresh timeout after headers would let a stalled response occupy the
        // single-flight slot indefinitely.
        self.exchange_once(Instant::now() + EXCHANGE_TIMEOUT).await
    }

    async fn exchange_once(&self, deadline: Instant) -> Result<CachedToken> {
        let body = form_body(&self.client_id, self.client_secret.expose(), &self.scopes);
        let response = timeout_at(
            deadline,
            self.http
                .post(self.endpoint.as_str())
                .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(ACCEPT, "application/json")
                .body(body)
                .send()
                .with_subscriber(tracing::subscriber::NoSubscriber::default()),
        )
        .await
        .map_err(|_| anyhow!("token exchange timed out"))?
        // reqwest errors carry the URL, so they become static text.
        .map_err(|_| anyhow!("token endpoint request failed"))?;
        let status = response.status();
        if status.is_redirection() {
            bail!(
                "{}",
                token::status_reason(status, "redirects are not allowed")
            );
        }
        let mut response = response;
        let body = match timeout_at(
            deadline,
            token::read_token_body(&mut response, MAX_RESPONSE_BODY_BYTES),
        )
        .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => bail!("{}", token::body_read_error(status, error)),
            Err(_) => bail!("{}", token::body_timeout_error(status)),
        };
        if !status.is_success() {
            bail!("{}", status_error(status, &body));
        }
        parse_token_response(&body)
    }
}

/// Lock, recovering the data after a panic in another thread: the cache
/// holds the last consistent token state, which is exactly what a
/// recovering caller should see.
fn lock<T>(lockable: &Mutex<T>) -> MutexGuard<'_, T> {
    lockable
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[async_trait]
impl UpstreamAuth for ClientCredentialsProvider {
    fn apply(&self, config: &mut StreamableHttpClientTransportConfig) {
        if let Some(header) = self.core.current_header() {
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
        Core::acquire_or_join(&self.core, Why::Proactive).await
    }

    fn refreshable(&self) -> bool {
        true
    }

    async fn refresh_after_rejection(&self, rejected: Option<&HeaderValue>) -> Result<()> {
        Core::acquire_or_join(&self.core, Why::Rejected(rejected)).await
    }

    fn reconnect_message(&self) -> String {
        "client-credentials token refresh failed; check the OAuth client configuration and token endpoint"
            .into()
    }

    /// Redact the client secret (raw and form-encoded), the current token
    /// and a bounded set of retired tokens — raw and `Bearer` forms.
    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut secrets = {
            let state = lock(&self.core.state);
            state
                .cached
                .iter()
                .map(|token| &token.secrets)
                .chain(&state.retired)
                .flatten()
                .cloned()
                .collect::<Vec<_>>()
        };
        let secret = self.core.client_secret.expose();
        secrets.push(secret.clone());
        let encoded = form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>();
        if encoded != *secret {
            secrets.push(encoded);
        }
        redact_secrets(text, &secrets)
    }
}
