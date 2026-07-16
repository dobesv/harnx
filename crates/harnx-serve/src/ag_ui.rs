#![allow(dead_code)]

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::session_actor::{
    PromptResult, SessionCommand, SessionHandle, SessionInfo, SessionRegistry, SubscribeResult,
};

#[cfg(test)]
use ag_ui_core::event::RunStartedEvent;
use ag_ui_core::{
    event::{
        BaseEvent, CustomEvent, Event, MessagesSnapshotEvent, RunErrorEvent, StepFinishedEvent,
        StepStartedEvent, TextMessageContentEvent, TextMessageEndEvent, TextMessageStartEvent,
        ThinkingEndEvent, ThinkingStartEvent, ThinkingTextMessageContentEvent,
        ThinkingTextMessageEndEvent, ThinkingTextMessageStartEvent, ToolCallArgsEvent,
        ToolCallEndEvent, ToolCallResultEvent, ToolCallStartEvent,
    },
    types::{
        context::Context,
        ids::{MessageId, RunId, ThreadId, ToolCallId},
        input::RunAgentInput,
        message::{FunctionCall, Message as AgUiMessage, Role},
        tool::{Tool, ToolCall},
    },
    JsonValue,
};
use bytes::Bytes;
use harnx_core::{
    agent_config::AgentConfig,
    event::{
        AgentEvent, AgentSource, ContentBlock, ModelEvent, NoticeEvent, SessionEvent, ToolEvent,
        TurnEvent,
    },
    message::{Message as HistoryMsg, MessageContent, MessageRole},
};
use harnx_runtime::{
    config::{Agent, Config, GlobalConfig},
    AgentCallFn,
};
use http::{Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, StreamBody};
use hyper::body::Frame;
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

#[derive(serde::Deserialize)]
struct RelaxedRunAgentInput<TState = JsonValue> {
    #[serde(rename = "threadId", default)]
    thread_id: Option<String>,
    #[serde(rename = "runId", default)]
    run_id: Option<String>,
    messages: Vec<RelaxedAgUiMessage>,
    #[serde(default)]
    state: TState,
    #[serde(default)]
    tools: Vec<Tool>,
    #[serde(default)]
    context: Vec<Context>,
    #[serde(rename = "forwardedProps", default)]
    forwarded_props: serde_json::Map<String, serde_json::Value>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
enum RelaxedAgUiMessage {
    User {
        #[serde(default)]
        id: Option<String>,
        content: String,
    },
    Assistant {
        content: Option<String>,
        name: Option<String>,
    },
    System {
        content: String,
    },
    Developer {
        content: String,
    },
    Tool {
        #[serde(rename = "toolCallId", default)]
        tool_call_id: Option<String>,
        content: String,
    },
}

impl From<RelaxedAgUiMessage> for AgUiMessage {
    fn from(value: RelaxedAgUiMessage) -> Self {
        match value {
            RelaxedAgUiMessage::User { id: _, content } => AgUiMessage::User {
                id: MessageId::random(),
                content,
                name: None,
            },
            RelaxedAgUiMessage::Assistant { content, name } => AgUiMessage::Assistant {
                id: MessageId::random(),
                content,
                name,
                tool_calls: None,
            },
            RelaxedAgUiMessage::System { content } => AgUiMessage::System {
                id: MessageId::random(),
                content,
                name: None,
            },
            RelaxedAgUiMessage::Developer { content } => AgUiMessage::Developer {
                id: MessageId::random(),
                content,
                name: None,
            },
            RelaxedAgUiMessage::Tool {
                tool_call_id,
                content,
            } => AgUiMessage::Tool {
                id: MessageId::random(),
                content,
                tool_call_id: tool_call_id
                    .as_deref()
                    .map(|id| {
                        serde_json::from_value(json!(id))
                            .expect("tool call id should deserialize from string")
                    })
                    .unwrap_or_else(ToolCallId::random),
                error: None,
            },
        }
    }
}

const THREAD_ID_NAMESPACE: Uuid = Uuid::from_u128(0x9f1f_5b4f_8080_4c1a_9544_1ce1_4b63_1a2f);
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
#[cfg(test)]
const TEST_SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(50);

pub type AppResponse = Response<BoxBody<Bytes, Infallible>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgUiError {
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl AgUiError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for AgUiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) | Self::NotFound(msg) | Self::Internal(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for AgUiError {}

/// Sink that maps harnx `AgentEvent`s to ag-ui-core `Event`s.
///
/// Phase-1 mapping: text message content and errors only.
/// Lifecycle events (RUN_STARTED, TEXT_MESSAGE_START, etc.) are handled by caller.
enum AgUiEventTx {
    Unbounded(UnboundedSender<Event>),
    Broadcast(broadcast::Sender<Event>),
}

impl AgUiEventTx {
    fn send(&self, event: Event) {
        match self {
            Self::Unbounded(tx) => {
                let _ = tx.send(event);
            }
            Self::Broadcast(tx) => {
                let _ = tx.send(event);
            }
        }
    }
}

impl From<UnboundedSender<Event>> for AgUiEventTx {
    fn from(value: UnboundedSender<Event>) -> Self {
        Self::Unbounded(value)
    }
}

impl From<broadcast::Sender<Event>> for AgUiEventTx {
    fn from(value: broadcast::Sender<Event>) -> Self {
        Self::Broadcast(value)
    }
}

#[derive(Debug, Clone)]
struct TextSegmentState {
    open_message_id: Option<MessageId>,
}

pub struct AgUiSink {
    tx: AgUiEventTx,
    message_id: MessageId,
    text_segment_state: Mutex<TextSegmentState>,
    history_snapshot: Option<Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>>,
    session_context: Option<Arc<dyn Fn() -> Option<UsageContextSnapshot> + Send + Sync>>,
    in_thinking_segment: std::sync::atomic::AtomicBool,
    turn_counter: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Clone)]
pub(crate) struct UsageContextSnapshot {
    pub(crate) context_tokens: usize,
    pub(crate) max_context_tokens: Option<usize>,
    pub(crate) context_percent: Option<f32>,
}

impl AgUiSink {
    pub fn new(tx: UnboundedSender<Event>, message_id: MessageId) -> Self {
        Self::with_snapshot_and_context(tx, message_id, true, None, None)
    }

    pub fn new_broadcast(tx: broadcast::Sender<Event>, message_id: MessageId) -> Self {
        Self::with_snapshot_and_context(tx, message_id, true, None, None)
    }

    pub fn new_broadcast_with_snapshot(
        tx: broadcast::Sender<Event>,
        message_id: MessageId,
        history_snapshot: Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>,
    ) -> Self {
        Self::with_snapshot_and_context(tx, message_id, true, Some(history_snapshot), None)
    }

    pub(crate) fn new_broadcast_with_snapshot_and_context(
        tx: broadcast::Sender<Event>,
        message_id: MessageId,
        history_snapshot: Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>,
        session_context: Arc<dyn Fn() -> Option<UsageContextSnapshot> + Send + Sync>,
    ) -> Self {
        Self::with_snapshot_and_context(
            tx,
            message_id,
            true,
            Some(history_snapshot),
            Some(session_context),
        )
    }

    fn with_snapshot(
        tx: impl Into<AgUiEventTx>,
        message_id: MessageId,
        text_message_started: bool,
        history_snapshot: Option<Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>>,
    ) -> Self {
        Self::with_snapshot_and_context(
            tx,
            message_id,
            text_message_started,
            history_snapshot,
            None,
        )
    }

    fn with_snapshot_and_context(
        tx: impl Into<AgUiEventTx>,
        message_id: MessageId,
        text_message_started: bool,
        history_snapshot: Option<Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>>,
        session_context: Option<Arc<dyn Fn() -> Option<UsageContextSnapshot> + Send + Sync>>,
    ) -> Self {
        let open_message_id = text_message_started.then_some(message_id.clone());
        Self {
            tx: tx.into(),
            message_id,
            text_segment_state: Mutex::new(TextSegmentState { open_message_id }),
            history_snapshot,
            session_context,
            in_thinking_segment: std::sync::atomic::AtomicBool::new(false),
            turn_counter: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn base_event() -> BaseEvent {
        BaseEvent {
            timestamp: None,
            raw_event: None,
        }
    }

    fn send(&self, event: Event) {
        self.tx.send(event);
    }

    fn tool_call_id(id: String) -> ToolCallId {
        serde_json::from_value(serde_json::Value::String(id))
            .expect("tool call id should deserialize from string")
    }

    fn ensure_text_message_started(&self) -> MessageId {
        let mut state = self.text_segment_state.lock().expect("text segment state");
        if let Some(message_id) = &state.open_message_id {
            return message_id.clone();
        }

        let message_id = MessageId::random();
        self.send(Event::TextMessageStart(TextMessageStartEvent {
            base: Self::base_event(),
            message_id: message_id.clone(),
            role: Role::Assistant,
        }));
        state.open_message_id = Some(message_id.clone());
        message_id
    }

    pub(crate) fn close_text_segment(&self) -> Option<MessageId> {
        let message_id = {
            let mut state = self.text_segment_state.lock().expect("text segment state");
            state.open_message_id.take()
        }?;

        self.send(Event::TextMessageEnd(TextMessageEndEvent {
            base: Self::base_event(),
            message_id: message_id.clone(),
        }));
        Some(message_id)
    }

    pub(crate) fn has_open_text_segment(&self) -> bool {
        self.text_segment_state
            .lock()
            .expect("text segment state")
            .open_message_id
            .is_some()
    }

    fn close_thinking_segment(&self) {
        if self
            .in_thinking_segment
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.send(Event::ThinkingTextMessageEnd(ThinkingTextMessageEndEvent {
                base: Self::base_event(),
            }));
            self.send(Event::ThinkingEnd(ThinkingEndEvent {
                base: Self::base_event(),
            }));
        }
    }

    fn emit_text_delta(&self, delta: String) {
        if delta.is_empty() {
            return;
        }
        self.close_thinking_segment();
        let message_id = self.ensure_text_message_started();
        self.send(Event::TextMessageContent(TextMessageContentEvent {
            base: Self::base_event(),
            message_id,
            delta,
        }));
    }

    fn emit_thinking_delta(&self, delta: String) {
        if delta.is_empty() {
            return;
        }
        if !self
            .in_thinking_segment
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.send(Event::ThinkingStart(ThinkingStartEvent {
                base: Self::base_event(),
                title: None,
            }));
            self.send(Event::ThinkingTextMessageStart(
                ThinkingTextMessageStartEvent {
                    base: Self::base_event(),
                },
            ));
        }
        self.send(Event::ThinkingTextMessageContent(
            ThinkingTextMessageContentEvent {
                base: Self::base_event(),
                delta,
            },
        ));
    }

    fn finish_turn(&self) {
        self.close_thinking_segment();
    }

    fn emit_custom(&self, name: impl Into<String>, value: serde_json::Value) {
        self.send(Event::Custom(CustomEvent {
            base: Self::base_event(),
            name: name.into(),
            value,
        }));
    }

    fn step_name_for_turn(&self) -> String {
        let turn = self.turn_counter.load(std::sync::atomic::Ordering::SeqCst);
        format!("turn-{turn}")
    }

    fn emit_history_snapshot(&self) {
        if let Some(history_snapshot) = &self.history_snapshot {
            self.send(snapshot_event(history_snapshot()));
        }
    }

    fn emit_tool_result(&self, tool_call_id: String, content: String) {
        self.close_thinking_segment();
        self.send(Event::ToolCallEnd(ToolCallEndEvent {
            base: Self::base_event(),
            tool_call_id: Self::tool_call_id(tool_call_id.clone()),
        }));
        self.send(Event::ToolCallResult(ToolCallResultEvent {
            base: Self::base_event(),
            message_id: MessageId::random(),
            tool_call_id: Self::tool_call_id(tool_call_id),
            content,
            role: Role::Tool,
        }));
    }

    fn session_usage_context(&self) -> Option<UsageContextSnapshot> {
        self.session_context
            .as_ref()
            .and_then(|session_context| session_context())
    }

    fn build_usage_payload(
        &self,
        input: u64,
        output: u64,
        cached: u64,
        session_label: Option<String>,
    ) -> serde_json::Value {
        let mut payload = json!({
            "input": input,
            "output": output,
            "cached": cached,
            "session_label": session_label,
        });
        if let Some(context) = self.session_usage_context() {
            payload["context_tokens"] = json!(context.context_tokens);
            payload["max_context_tokens"] = json!(context.max_context_tokens);
            if let Some(percent) = context.context_percent {
                payload["context_percent"] = json!(percent);
            }
        }
        payload
    }

    fn emit_tool_summary(&self, tool_call_id: String, markdown: String) {
        self.emit_custom(
            "tool_summary",
            json!({
                "tool_call_id": tool_call_id,
                "markdown": markdown,
            }),
        );
    }

    fn emit_tool_event(&self, event: ToolEvent) {
        self.close_thinking_segment();
        match event {
            ToolEvent::Started {
                id,
                name,
                markdown,
                input,
                ..
            } => {
                self.close_text_segment();
                self.send(Event::ToolCallStart(ToolCallStartEvent {
                    base: Self::base_event(),
                    tool_call_id: Self::tool_call_id(id.clone()),
                    tool_call_name: name,
                    parent_message_id: Some(self.message_id.clone()),
                }));
                if let Some(markdown) = markdown {
                    self.emit_tool_summary(id.clone(), markdown);
                }
                self.send(Event::ToolCallArgs(ToolCallArgsEvent {
                    base: Self::base_event(),
                    tool_call_id: Self::tool_call_id(id),
                    delta: input.to_string(),
                }));
            }
            ToolEvent::Completed {
                id,
                output,
                markdown,
            } => {
                let content = markdown.unwrap_or_else(|| {
                    output
                        .as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| output.to_string())
                });
                self.emit_tool_result(id, content);
            }
            ToolEvent::Failed { id, error } => {
                self.emit_tool_result(id, error);
            }
            ToolEvent::Blocked { id, reason, .. } => {
                self.emit_tool_result(id, reason);
            }
            ToolEvent::Progress { .. } | ToolEvent::Update { .. } => {}
        }
    }
}

impl harnx_core::event::AgentEventSink for AgUiSink {
    fn emit(&self, event: AgentEvent, _source: Option<AgentSource>) {
        match event {
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                let delta: String = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                if !delta.is_empty() {
                    self.close_thinking_segment();
                }
                self.emit_text_delta(delta);
            }
            AgentEvent::Model(ModelEvent::Final { output, .. }) => {
                self.emit_text_delta(output);
                self.finish_turn();
            }
            AgentEvent::Model(ModelEvent::Usage {
                input,
                output,
                cached,
                session_label,
            }) => {
                self.emit_custom(
                    "usage",
                    self.build_usage_payload(input, output, cached, session_label),
                );
            }
            AgentEvent::Model(ModelEvent::Error(message))
            | AgentEvent::Notice(NoticeEvent::Error(message)) => {
                self.finish_turn();
                self.send(Event::RunError(RunErrorEvent {
                    base: Self::base_event(),
                    message,
                    code: None,
                }));
            }
            AgentEvent::Tool(tool_event) => self.emit_tool_event(tool_event),
            AgentEvent::Model(ModelEvent::ThoughtChunk { blocks }) => {
                let delta: String = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                self.emit_thinking_delta(delta);
            }
            AgentEvent::Turn(TurnEvent::Started) => {
                let turn = self
                    .turn_counter
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                self.send(Event::StepStarted(StepStartedEvent {
                    base: Self::base_event(),
                    step_name: format!("turn-{turn}"),
                }));
            }
            AgentEvent::Turn(TurnEvent::Ended { outcome }) => {
                self.finish_turn();
                self.send(Event::StepFinished(StepFinishedEvent {
                    base: Self::base_event(),
                    step_name: self.step_name_for_turn(),
                }));
                if !outcome.output.is_empty()
                    || outcome.thought.is_some()
                    || outcome.handoff.is_some()
                    || outcome.usage.input_tokens > 0
                    || outcome.usage.output_tokens > 0
                    || outcome.usage.cached_tokens > 0
                {
                    self.emit_custom(
                        "turn_outcome",
                        serde_json::to_value(outcome).expect("turn outcome should serialize"),
                    );
                }
            }
            AgentEvent::Turn(TurnEvent::RetryAttempt { attempt, reason }) => {
                self.emit_custom(
                    "turn_retry_attempt",
                    json!({ "attempt": attempt, "reason": reason }),
                );
            }
            AgentEvent::Turn(TurnEvent::ModelFallback { from, to }) => {
                self.emit_custom("turn_model_fallback", json!({ "from": from, "to": to }));
            }
            AgentEvent::Turn(TurnEvent::HandoffRequested { agent, session_id }) => {
                self.emit_custom(
                    "turn_handoff_requested",
                    json!({ "agent": agent, "session_id": session_id }),
                );
            }
            AgentEvent::Session(SessionEvent::CompactingStarted) => {
                self.emit_custom("session_compacting_started", json!({}));
            }
            AgentEvent::Session(SessionEvent::CompactingCompleted) => {
                self.emit_custom("session_compacting_completed", json!({}));
                self.emit_history_snapshot();
            }
            AgentEvent::Session(SessionEvent::CompactingFailed(error)) => {
                self.emit_custom("session_compacting_failed", json!({ "error": error }));
            }
            AgentEvent::Session(SessionEvent::Saved { path }) => {
                self.emit_custom("session_saved", json!({ "path": path }));
            }
            AgentEvent::Session(SessionEvent::AgentInitializing { agent }) => {
                self.emit_custom("session_agent_initializing", json!({ "agent": agent }));
            }
            AgentEvent::Session(SessionEvent::ModelChanged { from, to }) => {
                self.emit_custom("session_model_changed", json!({ "from": from, "to": to }));
            }
            AgentEvent::Session(SessionEvent::RagIndexing { url, index, total }) => {
                self.emit_custom(
                    "session_rag_indexing",
                    json!({ "url": url, "index": index, "total": total }),
                );
            }
            AgentEvent::Plan { entries } => {
                self.emit_custom(
                    "plan",
                    serde_json::to_value(entries).expect("plan entries should serialize"),
                );
            }
            AgentEvent::Status(status) => {
                self.emit_custom("status", json!({ "text": status.text }));
            }
            AgentEvent::Session(SessionEvent::TitleUpdated(title)) => {
                self.emit_custom("session_title_updated", json!({ "title": title }))
            }
            AgentEvent::Session(SessionEvent::Generic { text }) => {
                self.finish_turn();
                self.emit_custom("session_generic", json!({ "text": text }));
            }
            AgentEvent::Session(SessionEvent::LogSeqAssigned { .. }) => {
                self.finish_turn();
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewMsg {
    pub role: Role,
    pub content: String,
}

pub fn frame_event(event: &Event) -> Result<String, AgUiError> {
    let json = serde_json::to_string(event)
        .map_err(|err| AgUiError::Internal(format!("failed to serialize AG-UI event: {err}")))?;
    Ok(format!("data: {json}\n\n"))
}

fn keep_alive_frame() -> &'static str {
    ": keep-alive\n\n"
}

fn keep_alive_stream(interval: Duration) -> impl tokio_stream::Stream<Item = Bytes> {
    tokio_stream::StreamExt::map(
        tokio_stream::StreamExt::skip(
            tokio_stream::wrappers::IntervalStream::new({
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                ticker
            }),
            1,
        ),
        |_| Bytes::from_static(keep_alive_frame().as_bytes()),
    )
}

pub fn parse_run_input(body: &[u8]) -> Result<RunAgentInput<JsonValue, JsonValue>, AgUiError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|err| AgUiError::BadRequest(format!("invalid AG-UI request body: {err}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| AgUiError::BadRequest("AG-UI body must be a JSON object".to_string()))?;
    let messages_value = obj.get("messages").cloned().ok_or_else(|| {
        AgUiError::BadRequest("AG-UI request must include a messages field".to_string())
    })?;
    let messages: Vec<AgUiMessage> =
        serde_json::from_value::<Vec<RelaxedAgUiMessage>>(messages_value)
            .map_err(|err| AgUiError::BadRequest(format!("invalid AG-UI messages: {err}")))?
            .into_iter()
            .map(AgUiMessage::from)
            .collect();

    let mut relaxed_value = value.clone();
    let relaxed_obj = relaxed_value
        .as_object_mut()
        .ok_or_else(|| AgUiError::BadRequest("AG-UI body must be a JSON object".to_string()))?;
    relaxed_obj.insert("messages".to_string(), serde_json::Value::Array(vec![]));
    relaxed_obj
        .entry("state".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    relaxed_obj
        .entry("tools".to_string())
        .or_insert_with(|| serde_json::Value::Array(vec![]));
    relaxed_obj
        .entry("context".to_string())
        .or_insert_with(|| serde_json::Value::Array(vec![]));
    relaxed_obj
        .entry("forwardedProps".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    let relaxed: RelaxedRunAgentInput<JsonValue> = serde_json::from_value(relaxed_value)
        .map_err(|err| AgUiError::BadRequest(format!("invalid AG-UI request body: {err}")))?;

    Ok(RunAgentInput {
        thread_id: relaxed
            .thread_id
            .as_deref()
            .and_then(|value| Uuid::parse_str(value).ok())
            .map(ThreadId::from)
            .unwrap_or_else(ThreadId::random),
        run_id: relaxed
            .run_id
            .as_deref()
            .and_then(|value| Uuid::parse_str(value).ok())
            .map(RunId::from)
            .unwrap_or_else(RunId::random),
        messages,
        state: relaxed.state,
        tools: relaxed.tools,
        context: relaxed.context,
        forwarded_props: JsonValue::Object(relaxed.forwarded_props),
    })
}

fn frame_run_boundary_event(event_type: &str, thread_id: &str, run_id: &str) -> String {
    let body = serde_json::json!({
        "type": event_type,
        "threadId": thread_id,
        "runId": run_id,
    });
    format!("data: {body}\n\n")
}

fn frame_run_error_event(thread_id: &str, run_id: &str, message: &str) -> String {
    let body = serde_json::json!({
        "type": "RUN_ERROR",
        "threadId": thread_id,
        "runId": run_id,
        "message": message,
    });
    format!("data: {body}\n\n")
}

#[derive(Clone, Copy)]
enum FirstRunState {
    AwaitingStarted,
    Active,
    Complete,
    Errored,
}

#[derive(Default)]
struct LiveStreamGuard {
    started_text_messages: HashSet<MessageId>,
    seen_tool_call_ids: Vec<ToolCallId>,
    started_steps: HashSet<String>,
    thinking_open: bool,
    thinking_text_open: bool,
}

fn frame_guarded_live_event(event: Event, guard: &mut LiveStreamGuard) -> Option<Bytes> {
    match event {
        Event::TextMessageStart(event) => {
            guard.started_text_messages.insert(event.message_id.clone());
            frame_event(&Event::TextMessageStart(event))
                .ok()
                .map(Bytes::from)
        }
        Event::TextMessageContent(event) => {
            let mut frames = String::new();
            if guard.started_text_messages.insert(event.message_id.clone()) {
                frames.push_str(
                    &frame_event(&Event::TextMessageStart(TextMessageStartEvent {
                        base: BaseEvent {
                            timestamp: None,
                            raw_event: None,
                        },
                        message_id: event.message_id.clone(),
                        role: Role::Assistant,
                    }))
                    .ok()?,
                );
            }
            frames.push_str(&frame_event(&Event::TextMessageContent(event)).ok()?);
            Some(Bytes::from(frames))
        }
        Event::TextMessageEnd(event) => {
            let mut frames = String::new();
            if guard.started_text_messages.insert(event.message_id.clone()) {
                frames.push_str(
                    &frame_event(&Event::TextMessageStart(TextMessageStartEvent {
                        base: BaseEvent {
                            timestamp: None,
                            raw_event: None,
                        },
                        message_id: event.message_id.clone(),
                        role: Role::Assistant,
                    }))
                    .ok()?,
                );
            }
            frames.push_str(&frame_event(&Event::TextMessageEnd(event.clone())).ok()?);
            guard.started_text_messages.remove(&event.message_id);
            Some(Bytes::from(frames))
        }
        Event::ToolCallStart(event) => {
            guard.seen_tool_call_ids.push(event.tool_call_id.clone());
            frame_event(&Event::ToolCallStart(event))
                .ok()
                .map(Bytes::from)
        }
        Event::ToolCallArgs(event) => guard
            .seen_tool_call_ids
            .iter()
            .any(|seen| seen == &event.tool_call_id)
            .then(|| frame_event(&Event::ToolCallArgs(event)).ok())
            .flatten()
            .map(Bytes::from),
        Event::ToolCallEnd(event) => guard
            .seen_tool_call_ids
            .iter()
            .any(|seen| seen == &event.tool_call_id)
            .then(|| frame_event(&Event::ToolCallEnd(event)).ok())
            .flatten()
            .map(Bytes::from),
        Event::ToolCallResult(event) => guard
            .seen_tool_call_ids
            .iter()
            .any(|seen| seen == &event.tool_call_id)
            .then(|| frame_event(&Event::ToolCallResult(event)).ok())
            .flatten()
            .map(Bytes::from),
        Event::StepStarted(event) => {
            guard.started_steps.insert(event.step_name.clone());
            frame_event(&Event::StepStarted(event))
                .ok()
                .map(Bytes::from)
        }
        Event::StepFinished(event) => {
            let mut frames = String::new();
            if guard.started_steps.insert(event.step_name.clone()) {
                frames.push_str(
                    &frame_event(&Event::StepStarted(StepStartedEvent {
                        base: BaseEvent {
                            timestamp: None,
                            raw_event: None,
                        },
                        step_name: event.step_name.clone(),
                    }))
                    .ok()?,
                );
            }
            frames.push_str(&frame_event(&Event::StepFinished(event.clone())).ok()?);
            guard.started_steps.remove(&event.step_name);
            Some(Bytes::from(frames))
        }
        Event::ThinkingStart(event) => {
            guard.thinking_open = true;
            frame_event(&Event::ThinkingStart(event))
                .ok()
                .map(Bytes::from)
        }
        Event::ThinkingEnd(event) => {
            let mut frames = String::new();
            if !guard.thinking_open {
                frames.push_str(
                    &frame_event(&Event::ThinkingStart(ThinkingStartEvent {
                        base: BaseEvent {
                            timestamp: None,
                            raw_event: None,
                        },
                        title: None,
                    }))
                    .ok()?,
                );
            }
            frames.push_str(&frame_event(&Event::ThinkingEnd(event)).ok()?);
            guard.thinking_open = false;
            Some(Bytes::from(frames))
        }
        Event::ThinkingTextMessageStart(event) => {
            guard.thinking_text_open = true;
            frame_event(&Event::ThinkingTextMessageStart(event))
                .ok()
                .map(Bytes::from)
        }
        Event::ThinkingTextMessageContent(event) => {
            let mut frames = String::new();
            if !guard.thinking_open {
                frames.push_str(
                    &frame_event(&Event::ThinkingStart(ThinkingStartEvent {
                        base: BaseEvent {
                            timestamp: None,
                            raw_event: None,
                        },
                        title: None,
                    }))
                    .ok()?,
                );
                guard.thinking_open = true;
            }
            if !guard.thinking_text_open {
                frames.push_str(
                    &frame_event(&Event::ThinkingTextMessageStart(
                        ThinkingTextMessageStartEvent {
                            base: BaseEvent {
                                timestamp: None,
                                raw_event: None,
                            },
                        },
                    ))
                    .ok()?,
                );
                guard.thinking_text_open = true;
            }
            frames.push_str(&frame_event(&Event::ThinkingTextMessageContent(event)).ok()?);
            Some(Bytes::from(frames))
        }
        Event::ThinkingTextMessageEnd(event) => {
            let mut frames = String::new();
            if !guard.thinking_open {
                frames.push_str(
                    &frame_event(&Event::ThinkingStart(ThinkingStartEvent {
                        base: BaseEvent {
                            timestamp: None,
                            raw_event: None,
                        },
                        title: None,
                    }))
                    .ok()?,
                );
                guard.thinking_open = true;
            }
            if !guard.thinking_text_open {
                frames.push_str(
                    &frame_event(&Event::ThinkingTextMessageStart(
                        ThinkingTextMessageStartEvent {
                            base: BaseEvent {
                                timestamp: None,
                                raw_event: None,
                            },
                        },
                    ))
                    .ok()?,
                );
            }
            frames.push_str(&frame_event(&Event::ThinkingTextMessageEnd(event)).ok()?);
            guard.thinking_text_open = false;
            Some(Bytes::from(frames))
        }
        other => frame_event(&other).ok().map(Bytes::from),
    }
}

fn frame_live_event(
    event: Event,
    state: &mut FirstRunState,
    guard: &mut LiveStreamGuard,
    thread_id: &str,
    run_id: &str,
) -> Option<Bytes> {
    match *state {
        FirstRunState::AwaitingStarted => match event {
            Event::RunStarted(_) => {
                *state = FirstRunState::Active;
                None
            }
            Event::RunFinished(event) => {
                *state = FirstRunState::Complete;
                let body = serde_json::json!({
                    "type": "RUN_FINISHED",
                    "threadId": thread_id,
                    "runId": run_id,
                    "result": event.result,
                });
                Some(Bytes::from(format!("data: {body}\n\n")))
            }
            Event::RunError(err) => {
                *state = FirstRunState::Errored;
                Some(Bytes::from(frame_run_error_event(
                    thread_id,
                    run_id,
                    &err.message,
                )))
            }
            other => frame_guarded_live_event(other, guard),
        },
        FirstRunState::Active => match event {
            Event::RunStarted(_) => None,
            Event::RunFinished(event) => {
                *state = FirstRunState::Complete;
                let body = serde_json::json!({
                    "type": "RUN_FINISHED",
                    "threadId": thread_id,
                    "runId": run_id,
                    "result": event.result,
                });
                Some(Bytes::from(format!("data: {body}\n\n")))
            }
            Event::RunError(err) => {
                *state = FirstRunState::Errored;
                Some(Bytes::from(frame_run_error_event(
                    thread_id,
                    run_id,
                    &err.message,
                )))
            }
            other => frame_guarded_live_event(other, guard),
        },
        FirstRunState::Complete | FirstRunState::Errored => {
            // Terminal state: stop forwarding events. The stream must end after
            // RUN_FINISHED/RUN_ERROR so the client's runAgent() promise resolves.
            None
        }
    }
}

fn snapshot_event(messages: Vec<AgUiMessage>) -> Event {
    Event::MessagesSnapshot(MessagesSnapshotEvent {
        base: BaseEvent {
            timestamp: None,
            raw_event: None,
        },
        messages,
    })
}

fn last_user_prompt(run_input: &RunAgentInput<JsonValue, JsonValue>) -> Option<String> {
    match run_input.messages.last() {
        Some(AgUiMessage::User { content, .. }) if !content.trim().is_empty() => {
            Some(content.clone())
        }
        _ => None,
    }
}

async fn subscribe(handle: &SessionHandle) -> SubscribeResult {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Subscribe { reply: reply_tx })
        .await
        .expect("send subscribe");
    reply_rx.await.expect("recv subscribe")
}

async fn prompt(handle: &SessionHandle, text: &str) -> PromptResult {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Prompt {
            text: text.to_string(),
            options: crate::session_actor::SessionPromptOptions::default(),
            reply: reply_tx,
        })
        .await
        .expect("send prompt");
    reply_rx.await.expect("recv prompt")
}

async fn get_info(handle: &SessionHandle) -> SessionInfo {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(SessionCommand::Get { reply: reply_tx })
        .await
        .expect("send get");
    reply_rx.await.expect("recv get")
}

struct UnsubscribeOnDrop {
    handle: SessionHandle,
}

impl Drop for UnsubscribeOnDrop {
    fn drop(&mut self) {
        let handle = self.handle.clone();
        tokio::spawn(async move {
            let _ = handle.tx.send(SessionCommand::Unsubscribe).await;
        });
    }
}

/// Whether a session has a live run that a fresh (promptless) subscribe/reload
/// should FOLLOW rather than terminate.
///
/// Both `Running` and `Interrupted` are active: `Interrupted` means the run is
/// paused awaiting a tool approval (HITL). A reload during that window must
/// attach to the live broadcast so the pending approval prompt reappears — NOT
/// emit a synthetic RUN_FINISHED that would close the stream and drop the gate.
fn session_state_is_active(state: &crate::session_actor::SessionState) -> bool {
    matches!(
        state,
        crate::session_actor::SessionState::Running { .. }
            | crate::session_actor::SessionState::Interrupted { .. }
    )
}

/// Frame the live broadcast body of an active run WITHOUT a leading RUN_STARTED
/// boundary — the caller is responsible for emitting exactly one RUN_STARTED
/// before this body. Shared by the prompted and promptless-while-active paths
/// so a reload never sees a duplicate RUN_STARTED for the same run.
///
/// The body TERMINATES once the run reaches a terminal state (RUN_FINISHED /
/// RUN_ERROR). Otherwise the body stays open on the (now idle) broadcast
/// channel, the client's runAgent() promise never resolves, and the
/// assistant-ui thread stays `isRunning` forever.
///
/// Non-terminal frames are forwarded via `take_while` (which ENDS the stream —
/// and drops the broadcast subscription — as soon as a terminal event is seen,
/// without waiting for any further broadcast item), stashing the terminal frame
/// in a cell. That stashed terminal frame is then chained as the final item so
/// the response body closes immediately after RUN_FINISHED/RUN_ERROR.
fn build_live_event_body(
    run_id: &str,
    thread_id_text: &str,
    snapshot_frame: Option<Bytes>,
    live_stream: impl tokio_stream::Stream<Item = Event> + Send + Sync + 'static,
) -> impl tokio_stream::Stream<Item = Bytes> + Send + Sync + 'static {
    let run_id = run_id.to_string();
    let thread_id_text = thread_id_text.to_string();
    let terminal_frame: std::sync::Arc<std::sync::Mutex<Option<Bytes>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let live_stream = {
        let mut state = FirstRunState::AwaitingStarted;
        let mut guard = LiveStreamGuard::default();
        let terminal_frame = terminal_frame.clone();
        let framed = tokio_stream::StreamExt::map(live_stream, move |event| {
            let is_terminal = matches!(event, Event::RunFinished(_) | Event::RunError(_));
            let bytes = frame_live_event(event, &mut state, &mut guard, &thread_id_text, &run_id);
            (bytes, is_terminal)
        });
        // Stop when a terminal event arrives, capturing its frame to emit last.
        let body = tokio_stream::StreamExt::take_while(framed, move |(bytes, is_terminal)| {
            if *is_terminal {
                *terminal_frame.lock().expect("terminal frame lock") = bytes.clone();
                false // end the passthrough (drops the broadcast subscription)
            } else {
                true
            }
        });
        tokio_stream::StreamExt::filter_map(body, |(bytes, _)| bytes)
    };
    // Terminal frame (RUN_FINISHED / RUN_ERROR), appended after the passthrough ends.
    let terminal_stream = {
        let terminal_frame = terminal_frame.clone();
        tokio_stream::StreamExt::filter_map(tokio_stream::once(()), move |_| {
            terminal_frame.lock().expect("terminal frame lock").take()
        })
    };
    let live_stream = tokio_stream::StreamExt::chain(live_stream, terminal_stream);
    tokio_stream::StreamExt::chain(tokio_stream::iter(snapshot_frame), live_stream)
}

fn build_prompted_event_stream(
    run_id: &str,
    thread_id_text: &str,
    snapshot_frame: Option<Bytes>,
    live_stream: impl tokio_stream::Stream<Item = Event> + Send + Sync + 'static,
) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Bytes> + Send + Sync + 'static>> {
    let body = build_live_event_body(run_id, thread_id_text, snapshot_frame, live_stream);
    // No keep_alive for prompted runs — the stream must terminate so the client's
    // runAgent() promise resolves. Multiplayer/persistent watch is a separate endpoint.
    Box::pin(tokio_stream::StreamExt::chain(
        tokio_stream::once(Bytes::from(frame_run_boundary_event(
            "RUN_STARTED",
            thread_id_text,
            run_id,
        ))),
        body,
    ))
}

fn build_promptless_event_stream(
    run_id: &str,
    thread_id_text: &str,
    snapshot_frame: Option<Bytes>,
    live_stream: impl tokio_stream::Stream<Item = Event> + Send + Sync + 'static,
    is_active: bool,
) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Bytes> + Send + Sync + 'static>> {
    let started = tokio_stream::once(Bytes::from(frame_run_boundary_event(
        "RUN_STARTED",
        thread_id_text,
        run_id,
    )));

    if !is_active {
        // Idle session: hydrate history then emit a synthetic RUN_FINISHED so the
        // client's stream terminates cleanly.
        let hydrated = tokio_stream::StreamExt::chain(started, tokio_stream::iter(snapshot_frame));
        return Box::pin(tokio_stream::StreamExt::chain(
            hydrated,
            tokio_stream::once(Bytes::from(frame_run_boundary_event(
                "RUN_FINISHED",
                thread_id_text,
                run_id,
            ))),
        ));
    }

    // Active session (Running or Interrupted — e.g. page reload mid-run or during a
    // tool-approval wait): emit exactly ONE RUN_STARTED, hydrate history, then
    // follow the live broadcast body until the real terminal event.
    // `build_live_event_body` carries the snapshot_frame and does NOT emit its own
    // RUN_STARTED, so the client never sees a duplicate boundary.
    let body = build_live_event_body(run_id, thread_id_text, snapshot_frame, live_stream);
    Box::pin(tokio_stream::StreamExt::chain(started, body))
}

fn build_ag_ui_event_stream(
    handle: &SessionHandle,
    run_id: &str,
    thread_id_text: &str,
    snapshot: Vec<AgUiMessage>,
    events: broadcast::Receiver<Event>,
    has_prompt: Option<&str>,
    is_active: bool,
) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Bytes> + Send + Sync + 'static>> {
    let snapshot_frame = match frame_event(&snapshot_event(snapshot)) {
        Ok(frame) => Some(Bytes::from(frame)),
        Err(err) => {
            log::warn!("failed to serialize AG-UI snapshot frame: {err}");
            None
        }
    };
    let handle_for_lag = handle.clone();
    let live_stream = tokio_stream::StreamExt::then(BroadcastStream::new(events), move |item| {
        let handle = handle_for_lag.clone();
        async move {
            match item {
                Ok(event) => Some(event),
                Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(_)) => {
                    let info = get_info(&handle).await;
                    Some(snapshot_event(info.history_snapshot))
                }
            }
        }
    });
    let live_stream = tokio_stream::StreamExt::filter_map(live_stream, |event| event);
    match has_prompt {
        // A prompted run is a pure delta stream (RUN_STARTED -> TEXT_MESSAGE_*/... ->
        // RUN_FINISHED). We deliberately do NOT emit MESSAGES_SNAPSHOT here: the
        // snapshot is captured before the new prompt is recorded, so it would not
        // contain the just-sent user message, and a client that applies it mid-run
        // (assistant-ui's applyExternalMessages is a full replace) would wipe the
        // optimistically-appended user message and the streaming reply. Clients
        // hydrate via their own promptless subscribe stream instead (multiplayer-safe).
        Some(_) => build_prompted_event_stream(run_id, thread_id_text, None, live_stream),
        None => build_promptless_event_stream(
            run_id,
            thread_id_text,
            snapshot_frame,
            live_stream,
            is_active,
        ),
    }
}

pub async fn ag_ui_run_with_call_fn(
    _base_config: &Config,
    registry: &SessionRegistry,
    agent: &str,
    session: &str,
    req_body: &[u8],
    _call_fn: Option<AgentCallFn>,
) -> Result<AppResponse, AgUiError> {
    let relaxed_run_input: RelaxedRunAgentInput<JsonValue> = serde_json::from_slice(req_body)
        .map_err(|err| AgUiError::BadRequest(format!("invalid AG-UI request body: {err}")))?;
    let run_id = relaxed_run_input
        .run_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let run_input = parse_run_input(req_body)?;
    let key = crate::session_actor::SessionKey {
        agent: agent.to_string(),
        session: session.to_string(),
    };
    let handle = registry.get_or_spawn(key);
    let SubscribeResult { snapshot, events } = subscribe(&handle).await;
    let session_info = get_info(&handle).await;
    let is_active = session_state_is_active(&session_info.state);
    let has_prompt = last_user_prompt(&run_input);
    let thread_id = derive_thread_id(session);
    let thread_id_text = thread_id.to_string();

    if let Some(text) = has_prompt.as_deref() {
        let _ = prompt(&handle, text).await;
    }

    let event_stream = build_ag_ui_event_stream(
        &handle,
        &run_id,
        &thread_id_text,
        snapshot,
        events,
        has_prompt.as_deref(),
        is_active,
    );
    let unsubscribe_guard = UnsubscribeOnDrop {
        handle: handle.clone(),
    };
    let stream = tokio_stream::StreamExt::map(event_stream, move |frame| {
        let _guard = &unsubscribe_guard;
        Ok::<_, Infallible>(Frame::data(frame))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .header("X-Thread-Id", thread_id.to_string())
        .body(BodyExt::boxed(StreamBody::new(stream)))
        .map_err(|err| AgUiError::Internal(format!("failed to build AG-UI response: {err}")))
}

pub fn resolve_agent(config: &Config, name: &str) -> Result<AgentConfig, AgUiError> {
    config
        .retrieve_agent(name)
        .map(Agent::into_config)
        .map_err(|_| AgUiError::NotFound(format!("agent '{name}' not found")))
}

#[cfg(test)]
async fn emit_test_event(handle: &SessionHandle, event: Event) {
    handle
        .tx
        .send(SessionCommand::EmitTestEvent { event })
        .await
        .expect("send test event");
}

pub fn derive_thread_id(session_id: &str) -> ThreadId {
    let uuid = Uuid::parse_str(session_id)
        .unwrap_or_else(|_| Uuid::new_v5(&THREAD_ID_NAMESPACE, session_id.as_bytes()));
    ThreadId::from(uuid)
}

pub fn build_local_input(
    prompt_config: &GlobalConfig,
    agent_name: &str,
    _session_key: &str,
    prompt_text: &str,
) -> Result<harnx_runtime::config::Input, AgUiError> {
    let mut agent = prompt_config
        .read()
        .retrieve_agent(agent_name)
        .map_err(|e| AgUiError::Internal(format!("failed to retrieve agent: {e}")))?;
    if let Err(e) = harnx_runtime::config::agent::resolve_variables(&mut agent) {
        log::warn!("Failed to resolve variables for agent '{agent_name}': {e}");
    }

    let mut input = harnx_runtime::config::input::from_str(prompt_config, prompt_text, None);
    harnx_runtime::config::input::set_agent(&mut input, prompt_config, agent.into_config());
    Ok(input)
}

fn client_matches_history(client: &AgUiMessage, history: &HistoryMsg) -> bool {
    client_role(client) == ag_ui_role_for_history(history.role)
        && normalize_visible_text(&client_content(client).unwrap_or_default())
            == normalize_visible_text(&history_content_text(&history.content))
}

fn normalize_visible_text(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(stripped) = trimmed
        .strip_prefix("<think>")
        .and_then(|rest| rest.strip_suffix("</think>"))
    {
        stripped.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

fn ag_ui_role_for_history(role: MessageRole) -> Role {
    match role {
        MessageRole::System => Role::System,
        MessageRole::Assistant => Role::Assistant,
        MessageRole::User => Role::User,
        MessageRole::Tool => Role::Tool,
    }
}

fn as_new_msg(message: &AgUiMessage) -> Option<NewMsg> {
    let role = client_role(message);
    let content = client_content(message)?;
    Some(NewMsg { role, content })
}

fn client_role(message: &AgUiMessage) -> Role {
    match message {
        AgUiMessage::Developer { .. } => Role::Developer,
        AgUiMessage::System { .. } => Role::System,
        AgUiMessage::Assistant { .. } => Role::Assistant,
        AgUiMessage::User { .. } => Role::User,
        AgUiMessage::Tool { .. } => Role::Tool,
    }
}

fn client_content(message: &AgUiMessage) -> Option<String> {
    match message {
        AgUiMessage::Developer { content, .. }
        | AgUiMessage::System { content, .. }
        | AgUiMessage::User { content, .. }
        | AgUiMessage::Tool { content, .. } => Some(content.clone()),
        AgUiMessage::Assistant { content, .. } => content.clone(),
    }
}

fn history_content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::ToolCalls(tool_calls) => tool_calls.text.clone(),
        _ => content.to_text(),
    }
}

fn history_tool_call_id(stable_base: &str, index: usize, persisted_id: Option<&str>) -> ToolCallId {
    // Prefer the persisted tool-call id. When absent, derive a DETERMINISTIC id
    // from a stable per-message base (persisted message id, else its log
    // sequence, else its ordinal in the history) so the same tool call keeps the
    // same id across reloads — a random message id here would break @assistant-ui
    // re-attaching the tool result to its call on every hydration.
    let raw_id = persisted_id
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{stable_base}-tool-{index}"));
    serde_json::from_value(serde_json::Value::String(raw_id))
        .expect("tool call id should deserialize from string")
}

/// Deterministic per-message base key used to synthesize tool-call ids when no
/// persisted id exists. Falls back through: persisted message id → `seq:{n}`
/// (log sequence) → `ord:{n}` (position in the history slice). Never random, so
/// the derived tool-call ids are stable across session reloads.
fn history_stable_base(message: &HistoryMsg, ordinal: usize) -> String {
    if let Some(id) = message.id.as_deref().filter(|id| !id.is_empty()) {
        return id.to_string();
    }
    match message.log_seq {
        Some(seq) => format!("seq:{seq}"),
        None => format!("ord:{ordinal}"),
    }
}

pub(crate) fn history_messages_for_snapshot(history: &[HistoryMsg]) -> Vec<AgUiMessage> {
    let mut messages = Vec::with_capacity(history.len());
    for (ordinal, message) in history.iter().enumerate() {
        let role = ag_ui_role_for_history(message.role);
        let visible = history_content_text(&message.content);
        let id = message
            .id
            .as_ref()
            .and_then(|value| serde_json::from_value(serde_json::Value::String(value.clone())).ok())
            .unwrap_or_else(MessageId::random);
        // Stable base for synthesizing tool-call ids — never the random `id` above.
        let stable_base = history_stable_base(message, ordinal);
        match &message.content {
            MessageContent::ToolCalls(tool_calls) => {
                let assistant_tool_calls = tool_calls
                    .tool_results
                    .iter()
                    .enumerate()
                    .map(|(index, tool_result)| {
                        let tool_call_id = history_tool_call_id(
                            &stable_base,
                            index,
                            tool_result.call.id.as_deref(),
                        );
                        ToolCall {
                            id: tool_call_id,
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: tool_result.call.name.clone(),
                                arguments: serde_json::to_string(&tool_result.call.arguments)
                                    .expect("tool call args should serialize"),
                            },
                        }
                    })
                    .collect::<Vec<_>>();
                messages.push(AgUiMessage::Assistant {
                    id: id.clone(),
                    content: Some(visible),
                    name: None,
                    tool_calls: Some(assistant_tool_calls.clone()),
                });
                for (index, tool_result) in tool_calls.tool_results.iter().enumerate() {
                    let content = tool_result
                        .markdown
                        .clone()
                        .or_else(|| tool_result.output.as_str().map(ToOwned::to_owned))
                        .unwrap_or_else(|| tool_result.output.to_string());
                    messages.push(AgUiMessage::Tool {
                        id: MessageId::random(),
                        content,
                        tool_call_id: assistant_tool_calls[index].id.clone(),
                        error: None,
                    });
                }
            }
            _ => messages.push(AgUiMessage::new(role, id, visible)),
        }
    }
    messages
}

fn history_role_for_client(role: &Role) -> MessageRole {
    match role {
        Role::Developer | Role::System => MessageRole::System,
        Role::Assistant => MessageRole::Assistant,
        Role::User => MessageRole::User,
        Role::Tool => MessageRole::Tool,
    }
}

#[cfg(test)]
#[path = "ag_ui_tests.rs"]
mod tests;
