use super::*;

fn error(status: u16, message: &str) -> LlmError {
    LlmError {
        status,
        message: message.into(),
        retry_after: None,
    }
}

#[test]
fn classifies_provider_context_overflow_diagnostics() {
    for message in [
        "The input token count exceeds the maximum number of tokens allowed (1048576).",
        "context_length_exceeded",
        "This model's maximum context length is 128000 tokens. However, you requested 130000 tokens.",
        "BadRequestError: maximum context length is 202752 tokens (parameter=input_tokens, value=205000)",
        "prompt is too long: 213009 tokens > 200000 maximum",
        "CONTEXT_WINDOW_EXCEEDED",
        "input_token_limit_exceeded",
        "Input is too long for requested model.",
        "Input is too long",
    ] {
        for status in [400, 413] {
            assert!(error(status, message).is_context_overflow(), "{status}: {message}");
        }
    }
}

#[test]
fn excludes_other_statuses_and_token_related_failures() {
    for status in [200, 401, 403, 429, 500, 503] {
        for message in ["context_length_exceeded", "Input is too long"] {
            assert!(
                !error(status, message).is_context_overflow(),
                "{status}: {message}"
            );
        }
    }
    for message in [
        "max_tokens must be less than or equal to 8192",
        "rate limit exceeded: tokens per minute",
        "invalid_request_error: unsupported parameter input_tokens",
        "authentication failed",
        "request body too large",
        "server_error",
    ] {
        assert!(!error(400, message).is_context_overflow(), "{message}");
    }
}
