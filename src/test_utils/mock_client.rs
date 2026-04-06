use crate::client::{
    ChatCompletionsData, ChatCompletionsOutput, Client, ExtraConfig, Model, RequestPatch,
    SseEvent, SseHandler, ToolCall,
};
use crate::config::{Config, GlobalConfig};
use crate::tool::ToolDeclaration;
use crate::utils::create_abort_signal;

use anyhow::Result;
use parking_lot::RwLock;
use reqwest::Client as ReqwestClient;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum MockResponseEvent {
    Text(String),
    ToolCall(ToolCall),
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
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct MockTurn {
    events: Vec<MockResponseEvent>,
    output: Option<ChatCompletionsOutput>,
}

impl MockTurn {
    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            events: vec![MockResponseEvent::Text(text.into())],
            output: None,
        }
    }

    pub fn events(&self) -> &[MockResponseEvent] {
        &self.events
    }

    fn output(&self) -> ChatCompletionsOutput {
        self.output.clone().unwrap_or_else(|| {
            let mut output = ChatCompletionsOutput::default();
            for event in &self.events {
                match event {
                    MockResponseEvent::Text(text) => output.text.push_str(text),
                    MockResponseEvent::ToolCall(tool_call) => output.tool_calls.push(tool_call.clone()),
                }
            }
            output
        })
    }
}

#[derive(Debug, Clone)]
pub struct MockTurnBuilder {
    turn: MockTurn,
}

impl Default for MockTurnBuilder {
    fn default() -> Self {
        Self {
            turn: MockTurn::default(),
        }
    }
}

impl MockTurnBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_text_chunk(mut self, chunk: impl Into<String>) -> Self {
        self.turn.events.push(MockResponseEvent::Text(chunk.into()));
        self
    }

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

    pub fn output(mut self, output: ChatCompletionsOutput) -> Self {
        self.turn.output = Some(output);
        self
    }

    pub fn build(self) -> MockTurn {
        self.turn
    }
}

#[derive(Debug, Default)]
struct MockClientState {
    turns: VecDeque<MockTurn>,
    conversation_history: Vec<ChatCompletionsData>,
}

#[derive(Debug)]
pub struct MockClient {
    global_config: GlobalConfig,
    model: Model,
    name: String,
    extra_config: Option<ExtraConfig>,
    patch_config: Option<RequestPatch>,
    declared_tools: Vec<ToolDeclaration>,
    default_turn: Option<MockTurn>,
    state: RwLock<MockClientState>,
}

impl MockClient {
    pub fn builder() -> MockClientBuilder {
        MockClientBuilder::default()
    }

    pub fn conversation_history(&self) -> parking_lot::RwLockReadGuard<'_, MockClientState> {
        self.state.read()
    }

    pub fn remaining_turns(&self) -> usize {
        self.state.read().turns.len()
    }

    fn next_turn(&self, data: ChatCompletionsData) -> MockTurn {
        let mut state = self.state.write();
        state.conversation_history.push(data);
        state
            .turns
            .pop_front()
            .or_else(|| self.default_turn.clone())
            .unwrap_or_default()
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
        let turn = self.next_turn(data);
        Ok(turn.output())
    }

    async fn chat_completions_streaming_inner(
        &self,
        _client: &ReqwestClient,
        handler: &mut SseHandler,
        data: ChatCompletionsData,
    ) -> Result<()> {
        let turn = self.next_turn(data);
        for event in turn.events() {
            event.apply(handler, &mut ChatCompletionsOutput::default())?;
        }
        handler.done();
        Ok(())
    }
}

#[derive(Debug)]
pub struct MockClientBuilder {
    global_config: GlobalConfig,
    model: Model,
    name: String,
    extra_config: Option<ExtraConfig>,
    patch_config: Option<RequestPatch>,
    declared_tools: Vec<ToolDeclaration>,
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
            declared_tools: vec![],
            turns: vec![],
            default_turn: None,
        }
    }
}

impl MockClientBuilder {
    pub fn global_config(mut self, global_config: GlobalConfig) -> Self {
        self.global_config = global_config;
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn model(mut self, model: Model) -> Self {
        self.model = model;
        self
    }

    pub fn extra_config(mut self, extra_config: ExtraConfig) -> Self {
        self.extra_config = Some(extra_config);
        self
    }

    pub fn patch_config(mut self, patch_config: RequestPatch) -> Self {
        self.patch_config = Some(patch_config);
        self
    }

    pub fn tools(mut self, tools: Vec<ToolDeclaration>) -> Self {
        self.declared_tools = tools;
        self
    }

    pub fn add_text_chunk(mut self, chunk: impl Into<String>) -> Self {
        if self.turns.is_empty() {
            self.turns.push(MockTurn::default());
        }
        if let Some(turn) = self.turns.last_mut() {
            turn.events.push(MockResponseEvent::Text(chunk.into()));
        }
        self
    }

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

    pub fn add_turn(mut self, turn: MockTurn) -> Self {
        self.turns.push(turn);
        self
    }

    pub fn default_turn(mut self, turn: MockTurn) -> Self {
        self.default_turn = Some(turn);
        self
    }

    pub fn build(self) -> MockClient {
        MockClient {
            global_config: self.global_config,
            model: self.model,
            name: self.name,
            extra_config: self.extra_config,
            patch_config: self.patch_config,
            declared_tools: self.declared_tools,
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
    use crate::utils::AbortSignal;
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
