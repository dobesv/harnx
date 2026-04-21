use super::{call_chat_completions, call_chat_completions_streaming, Client};
use crate::config::{GlobalConfig, Input};
use crate::tool::ToolResult;
use crate::utils::{warning_text, AbortSignal};

use anyhow::Result;
use std::sync::Arc;

use harnx_engine::retry::{CallFuture, TurnContext};

pub use harnx_core::retry_config::ModelCooldownMap;

/// Token usage from a completion call.
pub use super::CompletionTokenUsage;

/// Emit a warning message as an `AgentEvent::Notice(Warning)` through the
/// process-wide `AgentEventSink`. If no sink is installed (e.g. a test
/// context that didn't set one up), fall back to stderr so the warning is
/// at least visible.
fn emit_retry_warning(msg: &str) {
    use harnx_core::event::{AgentEvent, NoticeEvent};
    let event = AgentEvent::Notice(NoticeEvent::Warning(msg.to_string()));
    if !crate::agent_event_sink::emit_agent_event(event) {
        eprintln!("{}", warning_text(&format!("⚠ {msg}")));
    }
}

/// Build a [`TurnContext`] snapshot from the global harnx config. The
/// resulting context holds a clone of the clients list plus a shared
/// handle to the cooldown map (so mutations propagate back to the
/// Config), and a warn callback that routes through harnx's UI /
/// stderr channel.
fn build_turn_context(config: &GlobalConfig) -> TurnContext {
    let cfg = config.read();
    TurnContext {
        default_model_id: cfg.model_id.clone(),
        clients: cfg.clients.clone(),
        model_cooldowns: cfg.model_cooldowns.clone(),
        warn_fn: Arc::new(|msg: &str| emit_retry_warning(msg)),
        // Route through harnx's `crate::client::init_client` wrapper so
        // the test-client override installed by `TestStateGuard` is
        // honoured. In production this delegates to
        // `harnx_client::init_client`; in tests it short-circuits to the
        // installed `MockClient`.
        init_client_fn: Arc::new(super::init_client),
    }
}

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
    call_with_retry_and_fallback_custom(input, config, abort_signal, |input, client, cfg, abort| {
        Box::pin(default_call_fn(input, client, cfg, abort))
    })
    .await
}

/// Like [`call_with_retry_and_fallback`] but accepts a custom call function.
///
/// The `call_fn` closure is invoked for each attempt and receives the current
/// `Input`, a reference to the resolved `Client`, the `GlobalConfig`, and the
/// `AbortSignal`.  This allows callers (e.g. the TUI) to supply their own
/// streaming implementation while still benefiting from the retry/fallback loop.
///
/// The 4-arg closure shape is a harnx convenience — the underlying engine
/// loop takes a 3-arg closure `(input, client, abort)` and this wrapper
/// adapts by capturing `config` in the forwarded closure.
pub async fn call_with_retry_and_fallback_custom<F>(
    input: &Input,
    config: &GlobalConfig,
    abort_signal: AbortSignal,
    call_fn: F,
) -> Result<(
    String,
    Option<String>,
    Vec<ToolResult>,
    CompletionTokenUsage,
)>
where
    F: for<'a> Fn(&'a Input, &'a dyn Client, &'a GlobalConfig, AbortSignal) -> CallFuture<'a>
        + Send
        + Sync
        + 'static,
{
    let turn_ctx = build_turn_context(config);
    // Adapter: the engine's call_fn signature drops `&GlobalConfig`.
    // The caller's 4-arg closure expects `&'a GlobalConfig` that lives
    // for the same `'a` as input/client. A direct borrow of the outer
    // `config` reference cannot satisfy the HRTB (the borrow has one
    // fixed lifetime, but the engine closure is `for<'a>`). Instead we
    // clone the Arc-backed `GlobalConfig` once per LLM attempt and move
    // the clone into an `async move` block so the returned future is
    // fully self-owning. `call_fn` itself is wrapped in an Arc so it
    // can be cheaply cloned into each per-attempt future.
    let call_fn = Arc::new(call_fn);
    let config_for_closure: GlobalConfig = config.clone();
    harnx_engine::retry::call_with_retry_and_fallback_custom(
        input,
        &turn_ctx,
        abort_signal,
        move |input, client, abort| {
            let call_fn = call_fn.clone();
            let config_owned = config_for_closure.clone();
            Box::pin(async move { call_fn(input, client, &config_owned, abort).await })
        },
    )
    .await
}

async fn default_call_fn(
    input: &Input,
    client: &dyn Client,
    config: &GlobalConfig,
    abort_signal: AbortSignal,
) -> Result<(
    String,
    Option<String>,
    Vec<ToolResult>,
    CompletionTokenUsage,
)> {
    if crate::config::input::stream(input, config) {
        call_chat_completions_streaming(input, client, config, abort_signal).await
    } else {
        call_chat_completions(input, true, false, client, config, abort_signal).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::LlmError;
    use crate::client::TestStateGuard;
    use crate::config::Config;
    use crate::test_utils::{MockClient, MockTurnBuilder};
    use crate::utils::create_abort_signal;
    use harnx_core::retry_config::RetryConfig;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use std::time::Duration;

    fn make_config() -> GlobalConfig {
        let config = Config {
            stream: false,
            ..Default::default()
        };
        Arc::new(RwLock::new(config))
    }

    fn make_input(config: &GlobalConfig) -> Input {
        crate::config::input::from_str(config, "hello", None)
    }

    /// Test-support wrapper around the engine's inner retry loop that
    /// builds a `TurnContext` from `GlobalConfig` so test code can keep
    /// its existing `&GlobalConfig` calling style. Uses the same
    /// Arc-clone HRTB-friendly adapter pattern as
    /// [`call_with_retry_and_fallback_custom`].
    async fn try_model_with_retries(
        input: &Input,
        client: &dyn Client,
        config: &GlobalConfig,
        retry_config: &RetryConfig,
        abort_signal: AbortSignal,
    ) -> Result<(
        String,
        Option<String>,
        Vec<ToolResult>,
        CompletionTokenUsage,
    )> {
        let turn_ctx = build_turn_context(config);
        let config_for_closure: GlobalConfig = config.clone();
        harnx_engine::retry::try_model_with_retries_custom(
            input,
            client,
            &turn_ctx,
            retry_config,
            abort_signal,
            &move |i, c, a| {
                let config_owned = config_for_closure.clone();
                Box::pin(async move { default_call_fn(i, c, &config_owned, a).await })
            },
        )
        .await
    }

    /// Test-only access to the engine's `compute_cooldown` with a
    /// `GlobalConfig` adapter so cooldown assertions stay readable.
    fn compute_cooldown(err: &anyhow::Error, config: &GlobalConfig, model_id: &str) -> Duration {
        let cfg = config.read();
        harnx_engine::retry::compute_cooldown(err, &cfg.clients, model_id)
    }

    fn find_llm_error(err: &anyhow::Error) -> Option<&LlmError> {
        harnx_engine::retry::find_llm_error(err)
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

        let client = crate::config::input::create_client(&input, &config).unwrap();
        let retry_config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result =
            try_model_with_retries(&input, client.as_ref(), &config, &retry_config, abort).await;
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

        let client = crate::config::input::create_client(&input, &config).unwrap();
        let retry_config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result =
            try_model_with_retries(&input, client.as_ref(), &config, &retry_config, abort).await;
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

        let client = crate::config::input::create_client(&input, &config).unwrap();
        let retry_config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result =
            try_model_with_retries(&input, client.as_ref(), &config, &retry_config, abort).await;
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

        let client = crate::config::input::create_client(&input, &config).unwrap();
        let retry_config = RetryConfig {
            attempts: 3,
            initial_delay_ms: 10,
            max_delay_ms: 100,
        };
        let result =
            try_model_with_retries(&input, client.as_ref(), &config, &retry_config, abort).await;
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
        assert_eq!(
            cooldown,
            Duration::from_secs(harnx_engine::retry::DEFAULT_COOLDOWN_SECS)
        );
    }

    #[tokio::test]
    async fn test_invalid_fallback_model_reports_warning_and_skips() {
        // Primary model fails with a retryable error; the only fallback is an
        // invalid model name that cannot be resolved.  The function should:
        //   1. Emit a warning about the invalid fallback model up-front.
        //   2. Still fail with the primary model's error (not silently succeed).
        let mock = Arc::new(
            MockClient::builder()
                .error_on_stream(
                    LlmError {
                        status: 429,
                        message: "Rate limit".into(),
                        retry_after: None,
                    }
                    .into(),
                )
                .build(),
        );
        let _guard = TestStateGuard::new(Some(mock)).await;

        let config = make_config();
        let mut input = make_input(&config);
        input
            .agent_mut()
            .set_model_fallbacks(vec!["nonexistent-client:bogus-model".to_string()]);

        let abort = create_abort_signal();
        let result = call_with_retry_and_fallback_custom(&input, &config, abort, |i, c, cfg, a| {
            Box::pin(default_call_fn(i, c, cfg, a))
        })
        .await;

        assert!(
            result.is_err(),
            "Expected error when primary fails and fallback is invalid"
        );

        // The invalid fallback should have been placed on infinite cooldown.
        assert!(
            config
                .read()
                .model_cooldowns
                .lock()
                .is_on_cooldown("nonexistent-client:bogus-model"),
            "Invalid fallback model should be on cooldown"
        );
    }
}
