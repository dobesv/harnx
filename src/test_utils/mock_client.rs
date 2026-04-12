//! Mock LLM client for testing streaming responses and tool calls.
//!
//! This module provides [`MockClient`] which implements the [`Client`] trait
//! and can be used to simulate LLM responses in tests.
//!
//! # Overview
//!
//! The mock client supports:
//! - Streaming text responses in chunks
//! - Tool call generation
//! - Multi-turn conversations
//! - Error injection for testing error handling
//!
//! # Example
//!
//! ```ignore
//! use harnx::test_utils::{MockClient, MockTurnBuilder};
//! use harnx::client::set_test_client;
//!
//! // Create a mock that streams a response with a tool call
//! let mock = MockClient::builder()
//!     .add_turn(
//!         MockTurnBuilder::new()
//!             .add_text_chunk("Let me search...")
//!             .add_tool_call("search", serde_json::json!({"query": "test"}))
//!             .build()
//!     )
//!     .add_turn(
//!         MockTurnBuilder::new()
//!             .add_text_chunk("Found 3 results!")
//!             .build()
//!     )
//!     .build();
//!
//! // Inject the mock for testing
//! set_test_client(Some(mock.clone()));
//!
//! // ... run your test code ...
//!
//! // Clean up
//! set_test_client(None);
//! ```

use crate::client::{
    ChatCompletionsData, ChatCompletionsOutput, Client, ExtraConfig, Model, RequestPatch, SseEvent,
    SseHandler, ToolCall,
};
use crate::config::{Config, GlobalConfig};
use crate::utils::create_abort_signal;

use anyhow::Result;
use parking_lot::RwLock;
use reqwest::Client as ReqwestClient;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum MockResponseEvent {
    /// A text chunk to stream to the client.
    Text(String),
    /// A tool call to include in the response.
    ToolCall(ToolCall),
    /// Return an error during streaming (for testing error scenarios).
    /// Uses `Arc<Error>` since `anyhow::Error` is not `Clone`.
    Error(Arc<anyhow::Error>),
}

impl MockResponseEvent {
    fn apply(&self, handler: &mut SseHandler, output: &mut ChatCompletionsOutput) -> Result<()> {
        match self {
            Self::Text(text) => {
                handler.text(text)?;
                output.text.push_str(text);
            }
            Self::ToolCall(tool_call) => {
                handler.tool_call(tool_call.clone())?;
                output.tool_calls.push(tool_call.clone());
            }
            Self::Error(err) => {
                // Try to downcast to a concrete error type that implements Clone.
                // For LlmError (the most common test case), reconstruct it to preserve
                // the error type through the anyhow chain.
                if let Some(llm_err) = err.downcast_ref::<crate::client::LlmError>() {
                    return Err(crate::client::LlmError {
                        status: llm_err.status,
                        message: llm_err.message.clone(),
                        retry_after: llm_err.retry_after,
                    }
                    .into());
                }
                // Fallback: create a new error from the display string
                let msg = err.to_string();
                return Err(anyhow::anyhow!("{}", msg));
            }
        }
        Ok(())
    }
}

/// A single turn in a mock conversation.
///
/// Each turn contains a sequence of events (text chunks, tool calls, or errors)
/// that will be streamed to the client.
///
/// Use [`MockTurnBuilder`] to construct turns.
#[derive(Debug, Clone, Default)]
pub struct MockTurn {
    events: Vec<MockResponseEvent>,
    output: Option<ChatCompletionsOutput>,
}

impl MockTurn {
    /// Create a turn with a single text response.
    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            events: vec![MockResponseEvent::Text(text.into())],
            output: None,
        }
    }

    /// Get the events in this turn.
    pub fn events(&self) -> &[MockResponseEvent] {
        &self.events
    }

    fn output(&self) -> Result<ChatCompletionsOutput> {
        if let Some(output) = &self.output {
            return Ok(output.clone());
        }
        let mut output = ChatCompletionsOutput::default();
        for event in &self.events {
            match event {
                MockResponseEvent::Text(text) => output.text.push_str(text),
                MockResponseEvent::ToolCall(tool_call) => output.tool_calls.push(tool_call.clone()),
                MockResponseEvent::Error(err) => {
                    if let Some(llm_err) = err.downcast_ref::<crate::client::LlmError>() {
                        return Err(crate::client::LlmError {
                            status: llm_err.status,
                            message: llm_err.message.clone(),
                            retry_after: llm_err.retry_after,
                        }
                        .into());
                    }
                    return Err(anyhow::anyhow!("{}", err));
                }
            }
        }
        Ok(output)
    }
}

/// Builder for constructing [`MockTurn`] instances.
///
/// # Example
///
/// ```ignore
/// let turn = MockTurnBuilder::new()
///     .add_text_chunk("Hello")
///     .add_text_chunk(" world!")
///     .add_tool_call("search", serde_json::json!({"query": "test"}))
///     .build();
/// ```
#[derive(Debug, Clone, Default)]
pub struct MockTurnBuilder {
    turn: MockTurn,
}

impl MockTurnBuilder {
    /// Create a new builder with an empty turn.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a text chunk to the response.
    pub fn add_text_chunk(mut self, chunk: impl Into<String>) -> Self {
        self.turn.events.push(MockResponseEvent::Text(chunk.into()));
        self
    }

    /// Add a tool call to the response.
    ///
    /// A unique ID will be generated automatically.
    pub fn add_tool_call(mut self, name: impl Into<String>, arguments: Value) -> Self {
        let next_id = format!("tool-call-{}", self.turn.events.len() + 1);
        self.turn
            .events
            .push(MockResponseEvent::ToolCall(ToolCall::new(
                name.into(),
                arguments,
                Some(next_id),
                None,
            )));
        self
    }

    /// Add a tool call with a specific ID.
    pub fn add_tool_call_with_id(
        mut self,
        name: impl Into<String>,
        arguments: Value,
        id: impl Into<String>,
    ) -> Self {
        self.turn
            .events
            .push(MockResponseEvent::ToolCall(ToolCall::new(
                name.into(),
                arguments,
                Some(id.into()),
                None,
            )));
        self
    }

    /// Set the output directly (bypasses event streaming).
    pub fn output(mut self, output: ChatCompletionsOutput) -> Self {
        self.turn.output = Some(output);
        self
    }

    /// Build the turn.
    pub fn build(self) -> MockTurn {
        self.turn
    }
}

#[derive(Debug, Default)]
pub struct MockClientState {
    /// The remaining turns to be consumed.
    pub turns: VecDeque<MockTurn>,
    /// The full conversation history.
    pub conversation_history: Vec<ChatCompletionsData>,
}

/// Mock LLM client for testing.
///
/// Implements the [`Client`] trait and can simulate streaming responses,
/// tool calls, and error conditions.
///
/// Use [`MockClient::builder()`] to create instances.
///
/// # Example
///
/// ```ignore
/// let mock = MockClient::builder()
///     .add_turn(MockTurnBuilder::new().add_text_chunk("Hello!").build())
///     .build();
/// ```
#[derive(Debug)]
pub struct MockClient {
    global_config: GlobalConfig,
    model: Model,
    name: String,
    extra_config: Option<ExtraConfig>,
    patch_config: Option<RequestPatch>,
    default_turn: Option<MockTurn>,
    state: RwLock<MockClientState>,
}

impl MockClient {
    /// Create a new builder for constructing a mock client.
    pub fn builder() -> MockClientBuilder {
        MockClientBuilder::default()
    }

    /// Get the conversation history recorded by this mock.
    pub fn conversation_history(&self) -> parking_lot::RwLockReadGuard<'_, MockClientState> {
        self.state.read()
    }

    /// Get the number of remaining scripted turns.
    pub fn remaining_turns(&self) -> usize {
        self.state.read().turns.len()
    }

    fn next_turn(&self, data: ChatCompletionsData) -> Result<MockTurn> {
        let mut state = self.state.write();
        state.conversation_history.push(data);
        state
            .turns
            .pop_front()
            .or_else(|| self.default_turn.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "MockClient exhausted all {} scripted turns with no default",
                    state.conversation_history.len()
                )
            })
    }
}

#[async_trait::async_trait]
impl Client for MockClient {
    fn global_config(&self) -> &GlobalConfig {
        &self.global_config
    }

    fn extra_config(&self) -> Option<&ExtraConfig> {
        self.extra_config.as_ref()
    }

    fn patch_config(&self) -> Option<&RequestPatch> {
        self.patch_config.as_ref()
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &Model {
        &self.model
    }

    fn model_mut(&mut self) -> &mut Model {
        &mut self.model
    }

    async fn chat_completions_inner(
        &self,
        _client: &ReqwestClient,
        data: ChatCompletionsData,
    ) -> Result<ChatCompletionsOutput> {
        let turn = self.next_turn(data)?;
        turn.output()
    }

    async fn chat_completions_streaming_inner(
        &self,
        _client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let turn = self.next_turn(data)?;
        let mut result = Ok(());
        for event in turn.events() {
            if let Err(e) = event.apply(handler, &mut ChatCompletionsOutput::default()) {
                result = Err(e);
                break;
            }
        }
        handler.done();
        result
    }
}

/// Builder for constructing [`MockClient`] instances.
///
/// # Example
///
/// ```ignore
/// let mock = MockClient::builder()
///     .name("test-mock")
///     .add_turn(
///         MockTurnBuilder::new()
///             .add_text_chunk("Hello!")
///             .build()
///     )
///     .build();
/// ```
#[derive(Debug)]
pub struct MockClientBuilder {
    global_config: GlobalConfig,
    model: Model,
    name: String,
    extra_config: Option<ExtraConfig>,
    patch_config: Option<RequestPatch>,
    turns: Vec<MockTurn>,
    default_turn: Option<MockTurn>,
}

impl Default for MockClientBuilder {
    fn default() -> Self {
        Self {
            global_config: Arc::new(RwLock::new(Config::default())),
            model: Model::new("mock", "mock-model"),
            name: "mock".to_string(),
            extra_config: None,
            patch_config: None,
            turns: vec![],
            default_turn: None,
        }
    }
}

impl MockClientBuilder {
    /// Set the global config for the mock.
    pub fn global_config(mut self, global_config: GlobalConfig) -> Self {
        self.global_config = global_config;
        self
    }

    /// Set the name for the mock client.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set the model for the mock.
    pub fn model(mut self, model: Model) -> Self {
        self.model = model;
        self
    }

    /// Set extra configuration for the mock.
    pub fn extra_config(mut self, extra_config: ExtraConfig) -> Self {
        self.extra_config = Some(extra_config);
        self
    }

    /// Set the request patch configuration.
    pub fn patch_config(mut self, patch_config: RequestPatch) -> Self {
        self.patch_config = Some(patch_config);
        self
    }

    /// Add a text chunk to the last turn (creates a turn if needed).
    pub fn add_text_chunk(mut self, chunk: impl Into<String>) -> Self {
        if self.turns.is_empty() {
            self.turns.push(MockTurn::default());
        }
        if let Some(turn) = self.turns.last_mut() {
            turn.events.push(MockResponseEvent::Text(chunk.into()));
        }
        self
    }

    /// Add a tool call to the last turn (creates a turn if needed).
    pub fn add_tool_call(mut self, name: impl Into<String>, arguments: Value) -> Self {
        if self.turns.is_empty() {
            self.turns.push(MockTurn::default());
        }
        if let Some(turn) = self.turns.last_mut() {
            let next_id = format!("tool-call-{}", turn.events.len() + 1);
            turn.events.push(MockResponseEvent::ToolCall(ToolCall::new(
                name.into(),
                arguments,
                Some(next_id),
                None,
            )));
        }
        self
    }

    /// Add a scripted turn to the mock.
    ///
    /// Turns are consumed in order as the client makes requests.
    pub fn add_turn(mut self, turn: MockTurn) -> Self {
        self.turns.push(turn);
        self
    }

    /// Set a default turn for requests that exhaust scripted turns.
    pub fn default_turn(mut self, turn: MockTurn) -> Self {
        self.default_turn = Some(turn);
        self
    }

    /// Configure the mock to return an error on streaming requests.
    pub fn error_on_stream(mut self, error: anyhow::Error) -> Self {
        // Use a special marker in text to indicate error
        self.turns.push(MockTurn {
            events: vec![MockResponseEvent::Error(Arc::new(error))],
            output: None,
        });
        self
    }

    pub fn build(self) -> MockClient {
        MockClient {
            global_config: self.global_config,
            model: self.model,
            name: self.name,
            extra_config: self.extra_config,
            patch_config: self.patch_config,
            default_turn: self.default_turn,
            state: RwLock::new(MockClientState {
                turns: self.turns.into(),
                conversation_history: vec![],
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Message, MessageContent, MessageRole};
    use tokio::sync::mpsc::unbounded_channel;

    #[tokio::test(flavor = "multi_thread")]
    async fn mock_client_streams_expected_chunks() {
        let client = MockClient::builder()
            .add_turn(
                MockTurnBuilder::new()
                    .add_text_chunk("Hello")
                    .add_text_chunk(" ")
                    .add_text_chunk("world")
                    .build(),
            )
            .build();

        let data = ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text("Hi".to_string()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: true,
        };
        let reqwest_client = ReqwestClient::new();
        let (tx, mut rx) = unbounded_channel();
        let mut handler = SseHandler::new(tx, create_abort_signal());

        client
            .chat_completions_streaming_inner(&reqwest_client, &mut handler, data)
            .await
            .unwrap();

        let mut events = vec![];
        while let Some(event) = rx.recv().await {
            let is_done = matches!(event, SseEvent::Done);
            events.push(event);
            if is_done {
                break;
            }
        }

        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], SseEvent::Text(text) if text == "Hello"));
        assert!(matches!(&events[1], SseEvent::Text(text) if text == " "));
        assert!(matches!(&events[2], SseEvent::Text(text) if text == "world"));
        assert!(matches!(&events[3], SseEvent::Done));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mock_client_supports_multi_turn_responses() {
        let client = MockClient::builder()
            .add_turn(MockTurnBuilder::new().add_text_chunk("first").build())
            .add_turn(MockTurnBuilder::new().add_text_chunk("second").build())
            .build();
        let reqwest_client = ReqwestClient::new();

        let mk_data = |content: &str| ChatCompletionsData {
            messages: vec![Message::new(
                MessageRole::User,
                MessageContent::Text(content.to_string()),
            )],
            temperature: None,
            top_p: None,
            functions: None,
            stream: false,
        };

        let first = client
            .chat_completions_inner(&reqwest_client, mk_data("turn-1"))
            .await
            .unwrap();
        let second = client
            .chat_completions_inner(&reqwest_client, mk_data("turn-2"))
            .await
            .unwrap();

        assert_eq!(first.text, "first");
        assert_eq!(second.text, "second");
        let history = client.conversation_history();
        assert_eq!(history.conversation_history.len(), 2);
        assert!(matches!(
            &history.conversation_history[0].messages[0].content,
            MessageContent::Text(text) if text == "turn-1"
        ));
        assert!(matches!(
            &history.conversation_history[1].messages[0].content,
            MessageContent::Text(text) if text == "turn-2"
        ));
    }
}
