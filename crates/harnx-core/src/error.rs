//! Error types shared across client and engine layers.

use std::time::Duration;

/// Structured error from LLM API calls, carrying HTTP status and retry-after info.
#[derive(Debug)]
pub struct LlmError {
    pub status: u16,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl LlmError {
    /// Returns true for transient/retryable HTTP status codes.
    pub fn is_retryable(&self) -> bool {
        matches!(self.status, 429 | 500 | 502 | 503 | 529)
    }

    /// Provider rejected the input because it exceeds the context window.
    /// Checks HTTP 400/413 status and matches known error phrase patterns from
    /// OpenAI, Anthropic, Gemini, and Bedrock. Don't confuse rate limits or
    /// output-token validation with context overflow.
    ///
    /// Patterns (case-insensitive):
    /// - OpenAI: `context_length_exceeded`, `context_window_exceeded`,
    ///   `maximum context length is`
    /// - Anthropic: `prompt is too long`
    /// - Gemini: `the input token count exceeds the maximum number of tokens allowed`
    /// - Bedrock: `input is too long`, `input_token_limit_exceeded`
    pub fn is_context_overflow(&self) -> bool {
        if !matches!(self.status, 400 | 413) {
            return false;
        }
        let message = self.message.to_ascii_lowercase();
        [
            "context_length_exceeded",
            "context_window_exceeded",
            "input_token_limit_exceeded",
            "the input token count exceeds the maximum number of tokens allowed",
            "maximum context length is",
            "prompt is too long",
            "input is too long",
        ]
        .iter()
        .any(|pattern| message.contains(pattern))
    }

    /// Returns true for authentication/authorization errors (missing API key, etc).
    pub fn is_auth_error(&self) -> bool {
        matches!(self.status, 401 | 403)
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (status: {})", self.message, self.status)
    }
}

impl std::error::Error for LlmError {}

#[cfg(test)]
mod overflow_tests;
