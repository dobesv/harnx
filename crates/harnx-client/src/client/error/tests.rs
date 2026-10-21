use super::*;
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
