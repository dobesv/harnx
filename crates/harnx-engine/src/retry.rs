//! Retry/fallback orchestration helpers for LLM calls. Migrated from
//! `harnx::client::retry` per the monorepo-refactor plan. The UI-aware
//! LLM call wrappers (`call_chat_completions` /
//! `call_chat_completions_streaming`) and the retry *loop* itself
//! remain in harnx for now — this module supplies only the pure
//! helpers used by the loop.

use harnx_client::{retrieve_model, ClientConfig};
use harnx_core::error::LlmError;
use harnx_core::model::ModelType;
use harnx_core::retry_config::RetryConfig;
use std::time::Duration;

pub const DEFAULT_COOLDOWN_SECS: u64 = 60;

/// Compute exponential backoff delay for an attempt number.
/// Base = `config.initial_delay_ms`, doubling each attempt, capped at
/// `config.max_delay_ms`.
pub fn compute_backoff_delay(config: &RetryConfig, attempt: u32) -> Duration {
    let base = Duration::from_millis(config.initial_delay_ms);
    let max = Duration::from_millis(config.max_delay_ms);
    let delay = base.saturating_mul(2u32.saturating_pow(attempt));
    delay.min(max)
}

/// Compute cooldown duration for a model that has exhausted retries.
///
/// The error is consulted first (for `retry_after` hints and for an
/// effectively-infinite cooldown on auth errors); if the error carries
/// no retry hint, the model catalog's `error_cooldown_secs` wins; and
/// as a last resort we use [`DEFAULT_COOLDOWN_SECS`].
pub fn compute_cooldown(err: &anyhow::Error, clients: &[ClientConfig], model_id: &str) -> Duration {
    // Check for LlmError with retry_after from 429 headers
    if let Some(llm_err) = find_llm_error(err) {
        if llm_err.is_auth_error() {
            return Duration::from_secs(100 * 365 * 24 * 3600); // effectively infinite
        }
        if let Some(retry_after) = llm_err.retry_after {
            return retry_after;
        }
    }

    // Check model-specific error_cooldown_secs config
    if let Ok(model) = retrieve_model(clients, model_id, ModelType::Chat) {
        if let Some(cooldown_secs) = model.data().error_cooldown_secs {
            return Duration::from_secs(cooldown_secs);
        }
    }

    Duration::from_secs(DEFAULT_COOLDOWN_SECS)
}

/// Search the anyhow error chain for an `LlmError`.
/// The `Client` trait wraps errors with `.with_context()`, so the
/// `LlmError` may be a root cause rather than the top-level error.
pub fn find_llm_error(err: &anyhow::Error) -> Option<&LlmError> {
    for cause in err.chain() {
        if let Some(llm_err) = cause.downcast_ref::<LlmError>() {
            return Some(llm_err);
        }
    }
    None
}

/// Returns true if the error is non-retryable AND not an auth error.
/// These errors (400, 404) should not trigger fallback to next model.
pub fn is_non_retryable_non_auth(err: &anyhow::Error) -> bool {
    if let Some(llm_err) = find_llm_error(err) {
        !llm_err.is_retryable() && !llm_err.is_auth_error()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_delay() {
        let config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 1000,
            max_delay_ms: 60000,
        };
        assert_eq!(
            compute_backoff_delay(&config, 0),
            Duration::from_millis(1000)
        );
        assert_eq!(
            compute_backoff_delay(&config, 1),
            Duration::from_millis(2000)
        );
        assert_eq!(
            compute_backoff_delay(&config, 2),
            Duration::from_millis(4000)
        );
        assert_eq!(
            compute_backoff_delay(&config, 10),
            Duration::from_millis(60000)
        ); // clamped
    }
}
