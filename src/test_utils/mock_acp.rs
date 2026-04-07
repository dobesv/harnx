//! Mock ACP server for testing sub-agent delegation.
//!
//! This module provides types for configuring mock ACP responses.
//! The actual server/client setup uses `tokio::io::duplex` following the pattern
//! from `src/acp/server.rs` tests.

/// A builder for configuring mock ACP responses.
#[derive(Debug, Clone)]
pub struct MockAcpResponse {
    /// Text chunks to stream back as the response.
    pub chunks: Vec<String>,
    /// Tool calls to simulate (agent → sub-agent delegations).
    pub tool_calls: Vec<MockToolCall>,
}

#[derive(Debug, Clone)]
pub struct MockToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
    pub response: MockAcpResponse,
}

impl Default for MockAcpResponse {
    fn default() -> Self {
        Self {
            chunks: vec!["Done.".to_string()],
            tool_calls: vec![],
        }
    }
}

impl MockAcpResponse {
    /// Create a simple text response.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            chunks: vec![text.into()],
            tool_calls: vec![],
        }
    }

    /// Create a response with multiple chunks.
    pub fn chunks(chunks: Vec<String>) -> Self {
        Self {
            chunks,
            tool_calls: vec![],
        }
    }

    /// Add a tool call (sub-agent delegation) to the response.
    pub fn with_tool_call(mut self, call: MockToolCall) -> Self {
        self.tool_calls.push(call);
        self
    }
}

impl MockToolCall {
    /// Create a tool call that triggers a sub-agent.
    pub fn new(
        name: impl Into<String>,
        arguments: serde_json::Value,
        response: MockAcpResponse,
    ) -> Self {
        Self {
            name: name.into(),
            arguments,
            response,
        }
    }
}

/// Records of calls made to the mock ACP server.
#[derive(Debug, Clone)]
pub struct AcpCallRecord {
    pub session_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_response_builder() {
        let response = MockAcpResponse::text("Hello, world!");
        assert_eq!(response.chunks, vec!["Hello, world!"]);
        assert!(response.tool_calls.is_empty());
    }

    #[test]
    fn test_mock_response_with_tool_call() {
        let response = MockAcpResponse::text("Thinking...").with_tool_call(MockToolCall::new(
            "delegation",
            serde_json::json!({"agent": "subagent"}),
            MockAcpResponse::text("Sub-agent response"),
        ));

        assert_eq!(response.chunks, vec!["Thinking..."]);
        assert_eq!(response.tool_calls.len(), 1);
    }
}
