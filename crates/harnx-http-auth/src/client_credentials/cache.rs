//! Single-flight state shared by every caller of one provider.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::{anyhow, bail, Result};
use futures_util::FutureExt as _;
use reqwest::header::HeaderValue;
use tokio::{sync::Notify, time::Instant};

use super::{
    exchange::Grant,
    token::{is_fresh, CachedToken},
    AccessToken,
};

pub(super) struct Core {
    pub(super) grant: Grant,
    state: Mutex<RefreshState>,
}

/// Retired tokens that stay redactable after rotation. Upstream text can
/// echo a token that was in flight when a 401 landed, so a few recent
/// generations must remain redactable — but never an unbounded history.
const RETIRED_SECRET_GENERATIONS: usize = 4;

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

    fn has_usable_replacement(&self, rejected: Rejection<'_>) -> bool {
        self.cached.as_ref().is_some_and(|token| {
            !rejected.matches(&token.credential)
                && token
                    .expires_at
                    .is_none_or(|expiry| expiry > Instant::now())
        })
    }

    fn reject(&mut self, rejected: Rejection<'_>) -> bool {
        let retire = self.cached.as_ref().is_some_and(|token| match rejected {
            Rejection::Generation(_) => rejected.matches(&token.credential),
            Rejection::Header(_) => true,
        });
        if retire {
            self.retire_cached();
        }
        retire
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
                // Late 401s may reuse a replacement until actual expiry, not
                // just until the start of the proactive refresh window.
                if self.has_usable_replacement(rejected) {
                    return None;
                }
                // Only the MCP compatibility path retires even an expired
                // replacement: it compares headers rather than generations.
                self.reject(rejected);
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

#[derive(Clone, Copy)]
pub(super) enum Rejection<'a> {
    Generation(&'a AccessToken),
    Header(Option<&'a HeaderValue>),
}

impl Rejection<'_> {
    // qual:allow(iosp) reason: "Keep the two credential identity rules together: generation pointer versus sent header bytes."
    fn matches(self, token: &AccessToken) -> bool {
        match self {
            Self::Generation(sent) => Arc::ptr_eq(&sent.0, &token.0),
            Self::Header(Some(sent)) => sent.as_bytes() == token.0.as_bytes(),
            Self::Header(None) => true,
        }
    }
}

pub(super) enum Why<'a> {
    Proactive,
    Rejected(Rejection<'a>),
}

impl Core {
    pub(super) fn new(grant: Grant) -> Self {
        Self {
            grant,
            state: Mutex::default(),
        }
    }

    pub(super) fn current_token(&self) -> Option<AccessToken> {
        lock(&self.state)
            .cached
            .as_ref()
            .map(|token| token.credential.clone())
    }

    pub(super) fn reject(&self, sent: &AccessToken) -> bool {
        lock(&self.state).reject(Rejection::Generation(sent))
    }

    pub(super) fn token_secrets(&self) -> Vec<String> {
        let state = lock(&self.state);
        state
            .cached
            .iter()
            .map(|token| &token.secrets)
            .chain(&state.retired)
            .flatten()
            .cloned()
            .collect()
    }

    /// Admission has no await points. The first caller starts the detached
    /// runner before waiting, so cancelling that caller can't strand the slot.
    // qual:allow(iosp) reason: "Start a claimed exchange before the first await, but wait outside the admission lock."
    pub(super) async fn acquire_or_join(core: &Arc<Self>, why: Why<'_>) -> Result<()> {
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
            let outcome = match std::panic::AssertUnwindSafe(core.grant.exchange())
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
}

/// Lock, recovering the data after a panic in another thread: the cache
/// holds the last consistent token state, which is exactly what a
/// recovering caller should see.
fn lock<T>(lockable: &Mutex<T>) -> MutexGuard<'_, T> {
    lockable
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_credentials::token::parse_token_response;

    #[test]
    fn proactive_cache_hit_leaves_slot_open_for_mandatory_rejection() {
        let cached =
            parse_token_response(br#"{"access_token":"fresh","token_type":"Bearer"}"#).unwrap();
        let sent = cached.credential.authorization_header();
        let mut state = RefreshState {
            cached: Some(cached),
            ..RefreshState::default()
        };
        assert!(state.admit(Why::Proactive).is_none());
        assert!(state.inflight.is_none());
        assert!(matches!(
            state.admit(Why::Rejected(Rejection::Header(Some(&sent)))),
            Some((_, true))
        ));
        assert!(state.cached.is_none());
    }

    #[test]
    fn late_rejection_of_old_generation_never_retires_expired_replacement() {
        let expired_body = br#"{"access_token":"same","token_type":"Bearer","expires_in":0}"#;
        let sent = parse_token_response(expired_body).unwrap().credential;
        let mut state = RefreshState {
            cached: Some(parse_token_response(expired_body).unwrap()),
            ..RefreshState::default()
        };
        assert!(matches!(
            state.admit(Why::Rejected(Rejection::Generation(&sent))),
            Some((_, true))
        ));
        assert!(
            state.cached.is_some(),
            "only the sent generation can be retired"
        );
    }
}
