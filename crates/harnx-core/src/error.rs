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
