//! Retry/fallback orchestration for LLM calls. Migrated from
//! `harnx::client::retry` per spec step 7. The UI-aware LLM call
//! wrappers (`call_chat_completions` / `call_chat_completions_streaming`)
//! remain in harnx — this module supplies the config-agnostic retry
//! loop + helpers. Callers in harnx construct a [`TurnContext`] from
//! their own `GlobalConfig` and supply a `call_fn` closure that dispatches
//! to the UI-aware completion wrappers.

use anyhow::Result;
use harnx_client::{retrieve_model, Client, ClientConfig};
use harnx_core::abort::{wait_abort_signal, AbortSignal};
use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::error::LlmError;
use harnx_core::input::Input;
use harnx_core::model::ModelType;
use harnx_core::retry_config::{ModelCooldownMap, RetryConfig};
use harnx_core::tool::ToolCall;
use parking_lot::Mutex;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_COOLDOWN_SECS: u64 = 60;

/// Call-function future type used by the retry/fallback loop.
/// Returned by the `call_fn` closure supplied to
/// [`call_with_retry_and_fallback_custom`].
pub type CallFuture<'a> = Pin<
    Box<
        dyn std::future::Future<
                Output = Result<(String, Option<String>, Vec<ToolCall>, CompletionTokenUsage)>,
            > + Send
            + 'a,
    >,
>;

/// Type alias for a client-construction callback. Callers (harnx) inject
/// this so the engine can build clients without knowing about harnx's
/// test-client override (installed via `TestStateGuard`).
pub type InitClientFn = Arc<
    dyn Fn(&[ClientConfig], &harnx_core::model::Model) -> Result<Box<dyn Client>> + Send + Sync,
>;

/// Narrowed config view required by the retry/fallback loop. Callers
/// (in harnx) construct this from their own `GlobalConfig` and pass it
/// through to the engine.
///
/// `warn_fn` is invoked for operator-visible warnings (model cooldowns,
/// fallback exhaustion, etc.) — harnx supplies a callback that routes
/// into the TUI transcript or stderr.
///
/// `init_client_fn` is the client-constructor callback. harnx injects a
/// wrapper that honours its test-client override; production callers can
/// pass a plain `harnx_client::init_client` reference.
pub struct TurnContext {
    pub default_model_id: String,
    pub clients: Vec<ClientConfig>,
    pub model_cooldowns: Arc<Mutex<ModelCooldownMap>>,
    pub warn_fn: Arc<dyn Fn(&str) + Send + Sync>,
    pub init_client_fn: InitClientFn,
}

impl TurnContext {
    pub fn warn(&self, msg: &str) {
        (self.warn_fn)(msg);
    }

    fn init_client(&self, model: &harnx_core::model::Model) -> Result<Box<dyn Client>> {
        (self.init_client_fn)(&self.clients, model)
    }
}

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

/// Attempt an LLM call with retry (exponential backoff) and model fallback.
///
/// Iterates through the primary model and any fallbacks configured on the agent.
/// For each model, retries up to `retry_config.attempts` times on retryable errors.
/// On exhaustion or auth errors, sets a cooldown on the model and moves to the next.
///
/// The `call_fn` closure is invoked for each attempt and receives the current
/// `Input`, a reference to the resolved `Client`, and the `AbortSignal`. This
/// allows callers (e.g. the TUI) to supply their own streaming implementation
/// while still benefiting from the retry/fallback loop.
pub async fn call_with_retry_and_fallback_custom<F>(
    input: &Input,
    ctx: &TurnContext,
    abort_signal: AbortSignal,
    call_fn: F,
) -> Result<(String, Option<String>, Vec<ToolCall>, CompletionTokenUsage)>
where
    F: for<'a> Fn(&'a Input, &'a dyn Client, AbortSignal) -> CallFuture<'a>,
{
    let agent = input.agent();
    let retry_config = agent.retry_config();

    // Build model list: primary model + fallbacks
    let primary_model_id = agent
        .model_id()
        .unwrap_or(&ctx.default_model_id)
        .to_string();
    let mut model_ids: Vec<String> = vec![primary_model_id];
    model_ids.extend(agent.model_fallbacks().iter().cloned());

    // Eagerly validate all fallback model IDs so the user gets immediate
    // feedback about typos / missing models instead of a silent skip.
    for model_id in model_ids.iter().skip(1) {
        let valid = retrieve_model(&ctx.clients, model_id, ModelType::Chat).is_ok();
        if !valid {
            ctx.warn(&format!(
                "Invalid fallback model '{}' in agent config — this model does not exist and will be skipped.",
                model_id
            ));
            ctx.model_cooldowns.lock().set_infinite_cooldown(model_id);
        }
    }

    let mut last_error: Option<anyhow::Error> = None;

    for (idx, model_id) in model_ids.iter().enumerate() {
        // Skip models on cooldown
        if ctx.model_cooldowns.lock().is_on_cooldown(model_id) {
            continue;
        }

        // For the primary model (idx 0), use the already-resolved model from
        // the input's agent. For fallbacks, resolve from the model catalog.
        let client = if idx == 0 {
            match ctx.init_client(agent.model()) {
                Ok(client) => client,
                Err(err) => {
                    ctx.warn(&format!(
                        "Invalid model '{}': {}. This fallback will never be used — check your agent config.",
                        model_id, err
                    ));
                    ctx.model_cooldowns.lock().set_infinite_cooldown(model_id);
                    last_error = Some(err);
                    continue;
                }
            }
        } else {
            match resolve_client(ctx, model_id) {
                Ok(client) => client,
                Err(err) => {
                    ctx.warn(&format!(
                        "Invalid fallback model '{}': {}. This fallback will never be used — check your agent config.",
                        model_id, err
                    ));
                    ctx.model_cooldowns.lock().set_infinite_cooldown(model_id);
                    last_error = Some(err);
                    continue;
                }
            }
        };

        match try_model_with_retries_custom(
            input,
            client.as_ref(),
            ctx,
            &retry_config,
            abort_signal.clone(),
            &call_fn,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(err) => {
                if is_non_retryable_non_auth(&err) {
                    // Non-retryable errors (400, 404) should not sideline the model
                    return Err(err);
                }

                let cooldown = compute_cooldown(&err, &ctx.clients, model_id);
                ctx.model_cooldowns.lock().set_cooldown(model_id, cooldown);

                ctx.warn(&format!(
                    "Model '{}' exhausted retries (error: {}), cooldown {}s. Trying next fallback.",
                    model_id,
                    err,
                    cooldown.as_secs()
                ));
                // Yield so the TUI event loop has a chance to process the
                // warning message (emitted via the AgentEvent sink) before
                // the final error event arrives through a separate channel.
                // Without this yield, the error can race ahead of the last
                // "exhausted retries" notice event and appear in the wrong
                // order in the transcript.
                tokio::task::yield_now().await;
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All models are on cooldown")))
}

fn resolve_client(ctx: &TurnContext, model_id: &str) -> Result<Box<dyn Client>> {
    let model = retrieve_model(&ctx.clients, model_id, ModelType::Chat)?;
    ctx.init_client(&model)
}

/// Inner retry loop for a single model. Public for test-support use in
/// harnx — harnx retains a thin test wrapper that builds a `TurnContext`
/// from its `GlobalConfig` and calls this function.
pub async fn try_model_with_retries_custom<F>(
    input: &Input,
    client: &dyn Client,
    ctx: &TurnContext,
    retry_config: &RetryConfig,
    abort_signal: AbortSignal,
    call_fn: &F,
) -> Result<(String, Option<String>, Vec<ToolCall>, CompletionTokenUsage)>
where
    F: for<'a> Fn(&'a Input, &'a dyn Client, AbortSignal) -> CallFuture<'a>,
{
    let mut last_error: Option<anyhow::Error> = None;
    // Ensure at least one attempt even if configured as 0
    let attempts = retry_config.attempts.max(1);

    for attempt in 0..attempts {
        let result = call_fn(input, client, abort_signal.clone()).await;

        match result {
            Ok(result) => return Ok(result),
            Err(err) => {
                // Check if this is a retryable error
                if let Some(llm_err) = find_llm_error(&err) {
                    if llm_err.is_auth_error() {
                        // Auth errors: don't retry, let caller set infinite cooldown
                        return Err(err);
                    }
                    if !llm_err.is_retryable() {
                        // Non-retryable (400, 404, etc): fail immediately
                        return Err(err);
                    }
                    // Retryable error
                    if attempt + 1 < attempts {
                        let delay = compute_backoff_delay(retry_config, attempt);
                        ctx.warn(&format!(
                            "Retryable error (status {}, {}), attempt {}/{}. Retrying in {}ms...",
                            llm_err.status,
                            llm_err.message,
                            attempt + 1,
                            attempts,
                            delay.as_millis()
                        ));
                        tokio::select! {
                            () = tokio::time::sleep(delay) => {}
                            () = wait_abort_signal(&abort_signal) => {
                                return Err(err);
                            }
                        }
                    }
                } else {
                    // Non-LlmError (network timeout, DNS, etc): treat as retryable
                    if attempt + 1 < attempts {
                        let delay = compute_backoff_delay(retry_config, attempt);
                        ctx.warn(&format!(
                            "Network error, attempt {}/{}. Retrying in {}ms: {}",
                            attempt + 1,
                            attempts,
                            delay.as_millis(),
                            err
                        ));
                        tokio::select! {
                            () = tokio::time::sleep(delay) => {}
                            () = wait_abort_signal(&abort_signal) => {
                                return Err(err);
                            }
                        }
                    }
                }
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All retry attempts failed")))
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
