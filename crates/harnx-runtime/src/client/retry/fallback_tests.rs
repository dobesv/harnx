//! Exercise classification and request preparation through the production loop.
use super::{
    tests::{input_with_attempts, make_config, provider_switch_context},
    *,
};
use harnx_core::{
    abort::create_abort_signal,
    error::{LlmError, ModelFallbackReason},
};
use std::sync::atomic::{AtomicUsize, Ordering};

const MISSING_CREDENTIALS: u16 = u16::MAX;

fn failure(status: u16, message: &str) -> anyhow::Error {
    LlmError {
        status,
        message: message.into(),
        retry_after: None,
    }
    .into()
}

#[tokio::test]
async fn promoted_fallback_is_attempted_once_on_request_specific_failure() {
    let config = make_config();
    let mut input = input_with_attempts(&config, 1);
    let context = provider_switch_context();
    let model = harnx_client::retrieve_model(
        &context.clients,
        "gemini:gemini-2.5-pro",
        harnx_core::model::ModelType::Chat,
    )
    .unwrap();
    input.agent_mut().set_model(model);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let result = harnx_engine::retry::call_with_retry_and_fallback_custom(
        &mut input,
        &context,
        create_abort_signal(),
        move |_, client, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                assert_eq!(client.model().client_name(), "gemini");
                Err(failure(400, "context_length_exceeded"))
            })
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn unavailable_models_skip_retries_but_generic_bad_requests_stop_the_chain() {
    for unavailable in [false, true] {
        let config = make_config();
        let mut input = input_with_attempts(&config, 3);
        let context = provider_switch_context();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let result = harnx_engine::retry::call_with_retry_and_fallback_custom(
            &mut input,
            &context,
            create_abort_signal(),
            move |_, client, _| {
                let calls = seen.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    if client.model().client_name() == "gemini" {
                        return Ok(("ok".into(), None, vec![], Default::default()));
                    }
                    let error = failure(404, "provider diagnostic");
                    Err(if unavailable {
                        error.context(ModelFallbackReason::Unavailable)
                    } else {
                        error
                    })
                })
            },
        )
        .await;
        assert_eq!(result.is_ok(), unavailable);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            if unavailable { 2 } else { 1 }
        );
        assert_eq!(
            context
                .model_cooldowns
                .lock()
                .is_on_cooldown("claude:claude-sonnet"),
            unavailable
        );
    }
}

#[tokio::test]
async fn image_request_reaches_vision_fallback_intact_without_cooldown() {
    let config = make_config();
    let mut input = input_with_attempts(&config, 3);
    input.medias = vec!["data:image/png;base64,AQID".into()];
    let context = provider_switch_context();
    let cfg = config.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let result = harnx_engine::retry::call_with_retry_and_fallback_custom(
        &mut input,
        &context,
        create_abort_signal(),
        move |input, client, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            let config = cfg.clone();
            Box::pin(async move {
                let data = crate::config::input::prepare_completion_data(
                    input,
                    &config,
                    client.model(),
                    false,
                    client,
                )?;
                assert_eq!(client.model().client_name(), "gemini");
                assert!(data
                    .messages
                    .iter()
                    .any(|message| harnx_core::model::request_image_count(&message.content) == 1));
                Ok(("image seen".into(), None, vec![], Default::default()))
            })
        },
    )
    .await
    .unwrap();
    assert_eq!(result.0, "image seen");
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert!(!context
        .model_cooldowns
        .lock()
        .is_on_cooldown("claude:claude-sonnet"));
}

#[tokio::test]
async fn overflow_tries_larger_model_and_is_retained_for_compaction_on_exhaustion() {
    for fallback_status in [200, 401, 400, 413, 503, 0, MISSING_CREDENTIALS] {
        let config = make_config();
        let mut input = input_with_attempts(&config, 3);
        let context = provider_switch_context();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let result = harnx_engine::retry::call_with_retry_and_fallback_custom(
            &mut input,
            &context,
            create_abort_signal(),
            move |_, client, _| {
                seen.fetch_add(1, Ordering::Relaxed);
                if client.model().client_name() == "claude" {
                    Box::pin(async { Err(failure(400, "context_length_exceeded")) })
                } else {
                    fallback_response(fallback_status)
                }
            },
        )
        .await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            if matches!(fallback_status, 503 | 0) {
                4
            } else {
                2
            }
        );
        assert_fallback_result(fallback_status, result.map(|output| output.0), &input);
        assert!(!context
            .model_cooldowns
            .lock()
            .is_on_cooldown("claude:claude-sonnet"));
    }
}

fn fallback_response(status: u16) -> harnx_engine::retry::CallFuture<'static> {
    Box::pin(async move {
        match status {
            200 => Ok(("fits".into(), None, vec![], Default::default())),
            0 => Err(anyhow::anyhow!("Invalid provider event-stream")),
            MISSING_CREDENTIALS => {
                Err(anyhow::anyhow!("Miss 'api_key'").context(ModelFallbackReason::Unavailable))
            }
            413 => Err(failure(413, "input is too long")),
            _ => Err(failure(status, "invalid request or credentials")),
        }
    })
}

fn assert_fallback_result(status: u16, result: anyhow::Result<String>, input: &Input) {
    match status {
        200 => assert_eq!(result.unwrap(), "fits"),
        0 => {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("Invalid provider event-stream"));
        }
        400 | 503 => {
            // A malformed fallback must remain visible. Compaction would not fix
            // its parameters and must not hide that actionable provider error.
            let error = result.unwrap_err();
            assert!(!error
                .downcast_ref::<LlmError>()
                .unwrap()
                .is_context_overflow());
        }
        _ => {
            assert!(result
                .unwrap_err()
                .downcast_ref::<LlmError>()
                .unwrap()
                .is_context_overflow());
            assert_eq!(
                input.agent().model_id(),
                Some("claude:claude-sonnet"),
                "compaction must retry the model that overflowed"
            );
        }
    }
}
