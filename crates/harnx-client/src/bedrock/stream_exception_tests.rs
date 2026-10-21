use super::*;
use harnx_core::error::LlmError;

fn encodings(data: &Value) -> [Vec<u8>; 2] {
    let body = serde_json::to_vec(data).unwrap();
    let base64 = harnx_core::crypto::base64_encode(&body).into_bytes();
    [body, base64]
}

fn exception_error(payload: &[u8], kind: &str) -> LlmError {
    bedrock_stream_exception(payload, kind)
        .unwrap_err()
        .downcast::<LlmError>()
        .unwrap()
}

fn assert_status(error: &LlmError, kind: &str, status: u16) {
    assert_eq!(error.status, status, "{kind}: {error}");
    assert_eq!(error.is_retryable(), status == 500, "{kind}: {error}");
    assert_eq!(error.is_auth_error(), status == 403, "{kind}: {error}");
    assert!(error.message.contains(&format!("type: {kind}")), "{error}");
}

#[test]
fn bedrock_stream_exceptions_preserve_known_statuses_and_unknown_transience() {
    for (kind, message, status, overflow) in [
        ("validationException", "Invalid tool schema", 400, false),
        ("accessDeniedException", "Access denied for this model", 403, false),
        ("internalServerException", "Internal server failure", 500, false),
        ("unknownException", "Gateway failed", 500, false),
        ("validationException", "Input is too long for requested model.", 400, true),
        ("validationException", "Input is too long", 400, true),
        ("unknownException", "Input is too long", 400, true),
        ("accessDeniedException", "Access denied: Input is too long", 403, false),
    ] {
        for payload in encodings(&json!({"message": message})) {
            let error = exception_error(&payload, kind);
            assert_status(&error, kind, status);
            assert_eq!(error.is_context_overflow(), overflow, "{kind}: {error}");
            assert!(error.message.contains(message), "{error}");
        }
    }
}

#[test]
fn bedrock_stream_exception_without_string_message_keeps_parsed_payload() {
    for data in [
        json!({"detail": "Unsupported tool", "code": "invalid_tool", "param": "tools"}),
        json!({"message": null, "detail": "Permission missing"}),
        json!({"message": {"detail": "Malformed request"}}),
    ] {
        for payload in encodings(&data) {
            for (kind, status) in [("validationException", 400), ("accessDeniedException", 403), ("unknownException", 500)] {
                let error = exception_error(&payload, kind);
                assert_status(&error, kind, status);
                assert!(error.message.contains(&data.to_string()), "{error}");
                assert!(!error.is_context_overflow());
            }
        }
    }
}

#[test]
fn bedrock_stream_exception_keeps_unparseable_payload_diagnostic() {
    let payload = b"gateway returned a non-JSON error";
    let error = exception_error(payload, "validationException");
    assert_status(&error, "validationException", 400);
    assert!(error.message.contains(std::str::from_utf8(payload).unwrap()), "{error}");
}
