use super::{
    call_chat_completions, call_chat_completions_streaming, init_client, Client, LlmError, Model,
    ModelType,
};
use crate::config::{GlobalConfig, Input, RetryConfig};
use crate::tool::ToolResult;
use crate::utils::{wait_abort_signal, AbortSignal};

use anyhow::Result;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const DEFAULT_COOLDOWN_SECS: u64 = 60;

/// A far-future instant used as "infinite" cooldown (~100 years from process start).
static INFINITE_INSTANT: LazyLock<Instant> =
    LazyLock::new(|| Instant::now() + Duration::from_secs(100 * 365 * 24 * 3600));

#[derive(Debug, Clone, Default)]
pub struct ModelCooldownMap {
    cooldowns: HashMap<String, Instant>,
}

impl ModelCooldownMap {
    pub fn is_on_cooldown(&self, model_id: &str) -> bool {
        self.cooldowns
            .get(model_id)
            .is_some_and(|expires| Instant::now() < *expires)
    }

    pub fn set_cooldown(&mut self, model_id: &str, duration: Duration) {
        let expires = Instant::now()
            .checked_add(duration)
            .unwrap_or(*INFINITE_INSTANT);
        self.cooldowns.insert(model_id.to_string(), expires);
    }

    pub fn set_infinite_cooldown(&mut self, model_id: &str) {
        self.cooldowns
            .insert(model_id.to_string(), *INFINITE_INSTANT);
    }
}

/// Token usage from a completion call.
pub use super::CompletionTokenUsage;

/// Attempt an LLM call with retry (exponential backoff) and model fallback.
///
/// Iterates through the primary model and any fallbacks configured on the agent.
/// For each model, retries up to `retry_config.attempts` times on retryable errors.
/// On exhaustion or auth errors, sets a cooldown on the model and moves to the next.
pub async fn call_with_retry_and_fallback(
    input: &Input,
    config: &GlobalConfig,
    abort_signal: AbortSignal,
) -> Result<(
    String,
    Option<String>,
    Vec<ToolResult>,
    CompletionTokenUsage,
)> {
    let agent = input.agent();
    let retry_config = agent.retry_config();

    // Build model list: primary model + fallbacks
    let primary_model_id = {
        let cfg = config.read();
        agent.model_id().unwrap_or(&cfg.model_id).to_string()
    };
    let mut model_ids: Vec<String> = vec![primary_model_id];
    model_ids.extend(agent.model_fallbacks().iter().cloned());

    let mut last_error: Option<anyhow::Error> = None;

    for model_id in &model_ids {
        // Skip models on cooldown
        if config
            .read()
            .model_cooldowns
            .lock()
            .is_on_cooldown(model_id)
        {
            debug!("Skipping model '{}' (on cooldown)", model_id);
            continue;
        }

        // Resolve model and create client
        let client = match resolve_client(config, model_id) {
            Ok(client) => client,
            Err(err) => {
                warn!(
                    "Failed to initialize client for model '{}': {}. Setting infinite cooldown.",
                    model_id, err
                );
                config
                    .read()
                    .model_cooldowns
                    .lock()
                    .set_infinite_cooldown(model_id);
                last_error = Some(err);
                continue;
            }
        };

        match try_model_with_retries(input, client.as_ref(), &retry_config, abort_signal.clone())
            .await
        {
            Ok(result) => return Ok(result),
            Err(err) => {
                if is_non_retryable_non_auth(&err) {
                    // Non-retryable errors (400, 404) should not sideline the model
                    return Err(err);
                }

                let cooldown = compute_cooldown(&err, config, model_id);
                config
                    .read()
                    .model_cooldowns
                    .lock()
                    .set_cooldown(model_id, cooldown);

                warn!(
                    "Model '{}' exhausted retries, cooldown {}s. Trying next fallback.",
                    model_id,
                    cooldown.as_secs()
                );
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All models are on cooldown")))
}

fn resolve_client(config: &GlobalConfig, model_id: &str) -> Result<Box<dyn Client>> {
    let model = {
        let cfg = config.read();
        Model::retrieve_model(&cfg, model_id, ModelType::Chat)?
    };
    init_client(config, Some(model))
}

async fn try_model_with_retries(
    input: &Input,
    client: &dyn Client,
    retry_config: &RetryConfig,
    abort_signal: AbortSignal,
) -> Result<(
    String,
    Option<String>,
    Vec<ToolResult>,
    CompletionTokenUsage,
)> {
    let mut last_error: Option<anyhow::Error> = None;
    // Ensure at least one attempt even if configured as 0
    let attempts = retry_config.attempts.max(1);

    for attempt in 0..attempts {
        let result = if input.stream() {
            call_chat_completions_streaming(input, client, abort_signal.clone()).await
        } else {
            call_chat_completions(input, true, false, client, abort_signal.clone()).await
        };

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
                        warn!(
                            "Retryable error (status {}), attempt {}/{}. Retrying in {}ms...",
                            llm_err.status,
                            attempt + 1,
                            attempts,
                            delay.as_millis()
                        );
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
                        warn!(
                            "Network error, attempt {}/{}. Retrying in {}ms: {}",
                            attempt + 1,
                            attempts,
                            delay.as_millis(),
                            err
                        );
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

fn compute_backoff_delay(config: &RetryConfig, attempt: u32) -> Duration {
    let base = Duration::from_millis(config.initial_delay_ms);
    let max = Duration::from_millis(config.max_delay_ms);
    let delay = base.saturating_mul(2u32.saturating_pow(attempt));
    delay.min(max)
}

fn compute_cooldown(err: &anyhow::Error, config: &GlobalConfig, model_id: &str) -> Duration {
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
    let cfg = config.read();
    if let Ok(model) = Model::retrieve_model(&cfg, model_id, ModelType::Chat) {
        if let Some(cooldown_secs) = model.data().error_cooldown_secs {
            return Duration::from_secs(cooldown_secs);
        }
    }

    Duration::from_secs(DEFAULT_COOLDOWN_SECS)
}

/// Search the anyhow error chain for an LlmError.
/// The Client trait wraps errors with `.with_context()`, so the LlmError
/// may be a root cause rather than the top-level error.
fn find_llm_error(err: &anyhow::Error) -> Option<&LlmError> {
    for cause in err.chain() {
        if let Some(llm_err) = cause.downcast_ref::<LlmError>() {
            return Some(llm_err);
        }
    }
    None
}

/// Returns true if the error is non-retryable AND not an auth error.
/// These errors (400, 404) should not trigger fallback to next model.
fn is_non_retryable_non_auth(err: &anyhow::Error) -> bool {
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
    fn test_cooldown_map_basic() {
        let mut map = ModelCooldownMap::default();
        assert!(!map.is_on_cooldown("model-a"));

        map.set_cooldown("model-a", Duration::from_secs(60));
        assert!(map.is_on_cooldown("model-a"));
        assert!(!map.is_on_cooldown("model-b"));
    }

    #[test]
    fn test_cooldown_map_expired() {
        let mut map = ModelCooldownMap::default();
        // Set cooldown that has already expired (0 duration)
        map.cooldowns.insert(
            "model-a".to_string(),
            Instant::now() - Duration::from_secs(1),
        );
        assert!(!map.is_on_cooldown("model-a"));
    }

    #[test]
    fn test_infinite_cooldown() {
        let mut map = ModelCooldownMap::default();
        map.set_infinite_cooldown("model-a");
        assert!(map.is_on_cooldown("model-a"));
    }

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

    use crate::client::TestStateGuard;
    use crate::config::Config;
    use crate::test_utils::{MockClient, MockTurnBuilder};
    use crate::utils::create_abort_signal;
    use parking_lot::RwLock;
    use std::sync::Arc;

    fn make_config() -> GlobalConfig {
        let config = Config {
            stream: false,
            ..Default::default()
        };
        Arc::new(RwLock::new(config))
    }

    fn make_input(config: &GlobalConfig) -> Input {
        Input::from_str(config, "hello", None)
    }

    #[tokio::test]
    async fn test_retry_on_rate_limit_then_success() {
        let rate_limit_err: anyhow::Error = LlmError {
            status: 429,
            message: "Rate limit exceeded".to_string(),
            retry_after: None,
        }
        .into();
        let mock = Arc::new(
            MockClient::builder()
                .error_on_stream(rate_limit_err)
                .add_turn(MockTurnBuilder::new().add_text_chunk("Hello back!").build())
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;

        let config = make_config();
        let input = make_input(&config);
        let abort = create_abort_signal();

        let client = input.create_client().unwrap();
        let retry_config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result = try_model_with_retries(&input, client.as_ref(), &retry_config, abort).await;
        assert!(
            result.is_ok(),
            "Expected success after retries, got: {:?}",
            result.err()
        );
        let (output, _, _, _) = result.unwrap();
        assert_eq!(output, "Hello back!");
    }

    #[tokio::test]
    async fn test_auth_error_no_retry() {
        let auth_err: anyhow::Error = LlmError {
            status: 401,
            message: "Invalid API key".to_string(),
            retry_after: None,
        }
        .into();
        let mock = Arc::new(MockClient::builder().error_on_stream(auth_err).build());
        let _guard = TestStateGuard::new(Some(mock)).await;

        let config = make_config();
        let input = make_input(&config);
        let abort = create_abort_signal();

        let client = input.create_client().unwrap();
        let retry_config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result = try_model_with_retries(&input, client.as_ref(), &retry_config, abort).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        let llm_err = find_llm_error(&err).unwrap();
        assert!(llm_err.is_auth_error());
        assert_eq!(llm_err.status, 401);
    }

    #[tokio::test]
    async fn test_all_retries_exhausted_returns_error() {
        let mock = Arc::new(
            MockClient::builder()
                .error_on_stream(
                    LlmError {
                        status: 500,
                        message: "err".into(),
                        retry_after: None,
                    }
                    .into(),
                )
                .error_on_stream(
                    LlmError {
                        status: 500,
                        message: "err".into(),
                        retry_after: None,
                    }
                    .into(),
                )
                .error_on_stream(
                    LlmError {
                        status: 500,
                        message: "err".into(),
                        retry_after: None,
                    }
                    .into(),
                )
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;

        let config = make_config();
        let input = make_input(&config);
        let abort = create_abort_signal();

        let client = input.create_client().unwrap();
        let retry_config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result = try_model_with_retries(&input, client.as_ref(), &retry_config, abort).await;
        assert!(
            result.is_err(),
            "Expected error after all retries exhausted"
        );

        let err = result.unwrap_err();
        let llm_err = find_llm_error(&err);
        assert!(llm_err.is_some(), "Expected LlmError, got: {}", err);
        assert_eq!(llm_err.unwrap().status, 500);
    }

    #[tokio::test]
    async fn test_non_retryable_error_fails_immediately() {
        let bad_request_err: anyhow::Error = LlmError {
            status: 400,
            message: "Invalid request".to_string(),
            retry_after: None,
        }
        .into();
        let mock = Arc::new(
            MockClient::builder()
                .error_on_stream(bad_request_err)
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;

        let config = make_config();
        let input = make_input(&config);
        let abort = create_abort_signal();

        let client = input.create_client().unwrap();
        let retry_config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result = try_model_with_retries(&input, client.as_ref(), &retry_config, abort).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        let llm_err = find_llm_error(&err).unwrap();
        assert_eq!(llm_err.status, 400);
    }

    #[test]
    fn test_compute_cooldown_with_retry_after() {
        let err: anyhow::Error = LlmError {
            status: 429,
            message: "Rate limited".to_string(),
            retry_after: Some(Duration::from_secs(30)),
        }
        .into();
        let config = make_config();
        let cooldown = compute_cooldown(&err, &config, "some-model");
        assert_eq!(cooldown, Duration::from_secs(30));
    }

    #[test]
    fn test_compute_cooldown_auth_error() {
        let err: anyhow::Error = LlmError {
            status: 401,
            message: "Unauthorized".to_string(),
            retry_after: None,
        }
        .into();
        let config = make_config();
        let cooldown = compute_cooldown(&err, &config, "some-model");
        assert!(cooldown > Duration::from_secs(365 * 24 * 3600));
    }

    #[test]
    fn test_compute_cooldown_default() {
        let err: anyhow::Error = LlmError {
            status: 500,
            message: "Server error".to_string(),
            retry_after: None,
        }
        .into();
        let config = make_config();
        let cooldown = compute_cooldown(&err, &config, "unknown-model");
        assert_eq!(cooldown, Duration::from_secs(DEFAULT_COOLDOWN_SECS));
    }
}
