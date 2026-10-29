use super::*;
use crate::OpenAIClient;
use serde_json::json;

fn samples() -> Vec<Value> {
    vec![
        json!({"error": {"message": "The input token count exceeds the maximum number of tokens allowed (1048576).", "code": 400, "status": "INVALID_ARGUMENT"}}),
        json!({"error": {"message": "Invalid input", "type": "invalid_request_error", "code": "context_length_exceeded", "param": "messages"}}),
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": "prompt is too long: 213009 tokens > 200000 maximum"}}),
        json!({"error": {"type": "BadRequestError", "message": "This model's maximum context length is 202752 tokens. However, you requested 205000 tokens (parameter=input_tokens)."}}),
        json!({"message": "Input is too long for requested model."}),
    ]
}

fn assert_overflow(result: Result<()>) {
    let error = result.unwrap_err();
    let error = error.downcast_ref::<LlmError>().unwrap();
    assert!(error.is_context_overflow(), "{error}");
    assert!(!error.is_retryable());
}

#[test]
fn http_and_stream_overflow_mapping_preserve_provider_diagnostics() {
    for data in samples() {
        assert_overflow(catch_error(&data, 400, None));
        assert_overflow(catch_stream_error(&data));
    }
}

#[test]
fn openai_error_keeps_specific_code_alongside_generic_type() {
    let error = catch_error(&samples()[1], 400, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("type: invalid_request_error"));
    assert!(error.contains("code: context_length_exceeded"));
}

#[test]
fn unknown_stream_failures_keep_transient_behavior_and_http_statuses_are_not_rewritten() {
    let error = catch_stream_error(
        &json!({"error": {"message": "Internal error", "type": "server_error"}}),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<LlmError>().unwrap().is_retryable());
    let error = catch_error(&samples()[1], 401, None).unwrap_err();
    assert!(!error
        .downcast_ref::<LlmError>()
        .unwrap()
        .is_context_overflow());
}

#[test]
fn model_availability_is_classified_from_specific_provider_diagnostics() {
    for data in [
        json!({"error": {"code": "model_not_found", "message": "Unavailable model"}}),
        json!({"error": {"code": "1211", "message": "Unknown Model"}}),
        json!({"error": {"code": 1311, "message": "Your current subscription plan does not yet include access to glm-5.3"}}),
        json!({"error": {"type": "not_found_error", "message": "model: retired-model"}}),
        json!({"error": {"status": "NOT_FOUND", "message": "models/retired is not found for API version v1beta"}}),
        json!({"message": "The provided model identifier is invalid."}),
    ] {
        for error in [catch_error(&data, 400, None), catch_stream_error(&data)] {
            let error = error.unwrap_err().context("request failed");
            assert_eq!(
                error.downcast_ref::<ModelFallbackReason>(),
                Some(&ModelFallbackReason::Unavailable)
            );
            assert_eq!(error.downcast_ref::<LlmError>().unwrap().status, 400);
        }
    }
    let error = catch_error(
        &json!({"error": {"code": "model_not_found", "message": "unavailable"}}),
        422,
        None,
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ModelFallbackReason>(),
        Some(&ModelFallbackReason::Unavailable)
    );
}

#[test]
fn generic_request_errors_are_not_model_availability_errors() {
    for data in [
        json!({"error": {"type": "not_found_error", "message": "File not found"}}),
        json!({"error": {"status": "NOT_FOUND", "message": "Endpoint not found"}}),
        json!({"error": {"code": "1214", "message": "Invalid model parameter"}}),
        json!({"error": {"code": "1211", "message": "Invalid tool schema"}}),
        json!({"error": {"code": 1311, "message": "Invalid parameter"}}),
        json!({"error": {"code": "invalid_request_error", "message": "Invalid tools"}}),
        json!({"message": "Route not found"}),
    ] {
        for status in [400, 404] {
            let error = catch_error(&data, status, None).unwrap_err();
            assert!(error.downcast_ref::<ModelFallbackReason>().is_none());
            assert_eq!(error.downcast_ref::<LlmError>().unwrap().status, status);
        }
    }
}

#[test]
fn later_array_diagnostics_preserve_the_model_specific_error() {
    let diagnostic = json!({"code": "model_not_found", "message": "Selected model is unavailable"});
    for data in [
        json!({"errors": [{"message": "Request failed"}, diagnostic.clone()]}),
        json!([{"error": {"message": "Request failed"}}, {"error": diagnostic}]),
    ] {
        let error = catch_error(&data, 404, None).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ModelFallbackReason>(),
            Some(&ModelFallbackReason::Unavailable)
        );
        assert!(error
            .downcast_ref::<LlmError>()
            .unwrap()
            .message
            .contains("Selected model is unavailable"));
    }
}

#[test]
fn message_less_availability_diagnostic_keeps_other_provider_detail() {
    let data =
        json!({"errors": [{"code": "model_not_found"}, {"message": "deployment was removed"}]});
    let error = catch_error(&data, 404, None).unwrap_err();
    assert_eq!(
        error.downcast_ref::<ModelFallbackReason>(),
        Some(&ModelFallbackReason::Unavailable)
    );
    assert_eq!(
        error.downcast_ref::<LlmError>().unwrap().message,
        "deployment was removed"
    );
}

#[tokio::test]
async fn missing_provider_credentials_are_typed_without_an_http_request() {
    let config = harnx_core::provider_config::openai::OpenAIConfig {
        name: "missing_credentials_test".into(),
        ..Default::default()
    };
    let model = Model::new("missing_credentials_test", "gpt-4o");
    let client = OpenAIClient::from_config_for_test(config, model);
    let http = reqwest::Client::new();
    let data = ChatCompletionsData {
        messages: vec![],
        temperature: None,
        top_p: None,
        functions: None,
        stream: false,
        attachments_dir: None,
    };
    let error = client
        .chat_completions_inner(&http, data)
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ModelFallbackReason>(),
        Some(&ModelFallbackReason::Unavailable)
    );
    assert!(error.to_string().contains("api_key"));
    let temp = tempfile::tempdir().unwrap();
    let missing = Some(
        temp.path()
            .join("missing-auth.json")
            .to_string_lossy()
            .into_owned(),
    );
    let error =
        crate::codex_auth::prepare_codex_access_token(&http, "missing-codex-test", &missing)
            .await
            .err()
            .expect("missing Codex credentials must fail before HTTP");
    assert_eq!(
        error.downcast_ref::<ModelFallbackReason>(),
        Some(&ModelFallbackReason::Unavailable)
    );
}
