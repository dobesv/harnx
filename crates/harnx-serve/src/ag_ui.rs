#![allow(dead_code)]

use std::convert::Infallible;
use std::sync::Arc;
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
        StepStartedEvent, TextMessageContentEvent, TextMessageStartEvent, ThinkingEndEvent,
        ThinkingStartEvent, ThinkingTextMessageContentEvent, ThinkingTextMessageEndEvent,
        ThinkingTextMessageStartEvent, ToolCallArgsEvent, ToolCallEndEvent, ToolCallResultEvent,
        ToolCallStartEvent,
    },
    types::{
        context::Context,
        ids::{MessageId, RunId, ThreadId, ToolCallId},
        input::RunAgentInput,
        message::{Message as AgUiMessage, Role},
        tool::Tool,
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

pub struct AgUiSink {
    tx: AgUiEventTx,
    message_id: MessageId,
    history_snapshot: Option<Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>>,
    in_thinking_segment: std::sync::atomic::AtomicBool,
    text_message_started: std::sync::atomic::AtomicBool,
    turn_counter: std::sync::atomic::AtomicUsize,
}

impl AgUiSink {
    pub fn new(tx: UnboundedSender<Event>, message_id: MessageId) -> Self {
        Self::with_snapshot(tx, message_id, true, None)
    }

    pub fn new_broadcast(tx: broadcast::Sender<Event>, message_id: MessageId) -> Self {
        Self::with_snapshot(tx, message_id, true, None)
    }

    pub fn new_broadcast_with_snapshot(
        tx: broadcast::Sender<Event>,
        message_id: MessageId,
        history_snapshot: Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>,
    ) -> Self {
        Self::with_snapshot(tx, message_id, true, Some(history_snapshot))
    }

    fn with_snapshot(
        tx: impl Into<AgUiEventTx>,
        message_id: MessageId,
        text_message_started: bool,
        history_snapshot: Option<Arc<dyn Fn() -> Vec<AgUiMessage> + Send + Sync>>,
    ) -> Self {
        Self {
            tx: tx.into(),
            message_id,
            history_snapshot,
            in_thinking_segment: std::sync::atomic::AtomicBool::new(false),
            text_message_started: std::sync::atomic::AtomicBool::new(text_message_started),
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

    fn ensure_text_message_started(&self) {
        if !self
            .text_message_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.send(Event::TextMessageStart(TextMessageStartEvent {
                base: Self::base_event(),
                message_id: self.message_id.clone(),
                role: Role::Assistant,
            }));
        }
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
        self.ensure_text_message_started();
        self.send(Event::TextMessageContent(TextMessageContentEvent {
            base: Self::base_event(),
            message_id: self.message_id.clone(),
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

    fn emit_tool_event(&self, event: ToolEvent) {
        self.close_thinking_segment();
        match event {
            ToolEvent::Started {
                id, name, input, ..
            } => {
                self.send(Event::ToolCallStart(ToolCallStartEvent {
                    base: Self::base_event(),
                    tool_call_id: Self::tool_call_id(id.clone()),
                    tool_call_name: name,
                    parent_message_id: Some(self.message_id.clone()),
                }));
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
                    json!({
                        "input": input,
                        "output": output,
                        "cached": cached,
                        "session_label": session_label,
                    }),
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

fn frame_live_event(
    event: Event,
    state: &mut FirstRunState,
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
            other => frame_event(&other).ok().map(Bytes::from),
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
            other => frame_event(&other).ok().map(Bytes::from),
        },
        FirstRunState::Complete | FirstRunState::Errored => {
            frame_event(&event).ok().map(Bytes::from)
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
        Some(AgUiMessage::User { content, .. }) => Some(content.clone()),
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

fn build_prompted_event_stream(
    run_id: &str,
    thread_id_text: &str,
    snapshot_frame: Option<Bytes>,
    live_stream: impl tokio_stream::Stream<Item = Event> + Send + Sync + 'static,
) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Bytes> + Send + Sync + 'static>> {
    let run_id = run_id.to_string();
    let thread_id_text = thread_id_text.to_string();
    let run_id_for_closure = run_id.clone();
    let thread_id_text_for_closure = thread_id_text.clone();
    let live_stream = tokio_stream::StreamExt::filter_map(live_stream, {
        let mut state = FirstRunState::AwaitingStarted;
        move |event| {
            frame_live_event(
                event,
                &mut state,
                &thread_id_text_for_closure,
                &run_id_for_closure,
            )
        }
    });
    let remaining = tokio_stream::StreamExt::chain(tokio_stream::iter(snapshot_frame), live_stream);
    Box::pin(tokio_stream::StreamExt::chain(
        tokio_stream::once(Bytes::from(frame_run_boundary_event(
            "RUN_STARTED",
            &thread_id_text,
            &run_id,
        ))),
        futures_util::stream::select(remaining, keep_alive_stream(SSE_KEEPALIVE_INTERVAL)),
    ))
}

fn build_promptless_event_stream(
    run_id: &str,
    thread_id_text: &str,
    snapshot_frame: Option<Bytes>,
    live_stream: impl tokio_stream::Stream<Item = Event> + Send + Sync + 'static,
) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Bytes> + Send + Sync + 'static>> {
    let synthetic = tokio_stream::StreamExt::chain(
        tokio_stream::StreamExt::chain(
            tokio_stream::once(Bytes::from(frame_run_boundary_event(
                "RUN_STARTED",
                thread_id_text,
                run_id,
            ))),
            tokio_stream::iter(snapshot_frame),
        ),
        tokio_stream::once(Bytes::from(frame_run_boundary_event(
            "RUN_FINISHED",
            thread_id_text,
            run_id,
        ))),
    );
    let passthrough =
        tokio_stream::StreamExt::filter_map(live_stream, |event| match frame_event(&event) {
            Ok(frame) => Some(Bytes::from(frame)),
            Err(err) => {
                log::warn!("failed to serialize AG-UI passthrough frame: {err}");
                None
            }
        });
    Box::pin(futures_util::stream::select(
        tokio_stream::StreamExt::chain(synthetic, passthrough),
        keep_alive_stream(SSE_KEEPALIVE_INTERVAL),
    ))
}

fn build_ag_ui_event_stream(
    handle: &SessionHandle,
    run_id: &str,
    thread_id_text: &str,
    snapshot: Vec<AgUiMessage>,
    events: broadcast::Receiver<Event>,
    has_prompt: Option<&str>,
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
        Some(_) => build_prompted_event_stream(run_id, thread_id_text, snapshot_frame, live_stream),
        None => build_promptless_event_stream(run_id, thread_id_text, snapshot_frame, live_stream),
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

pub(crate) fn history_messages_for_snapshot(history: &[HistoryMsg]) -> Vec<AgUiMessage> {
    let mut messages = Vec::with_capacity(history.len());
    for message in history {
        let role = ag_ui_role_for_history(message.role);
        let visible = history_content_text(&message.content);
        let id = message
            .id
            .as_ref()
            .and_then(|value| serde_json::from_value(serde_json::Value::String(value.clone())).ok())
            .unwrap_or_else(MessageId::random);
        match &message.content {
            MessageContent::ToolCalls(tool_calls) => {
                if !visible.is_empty() {
                    messages.push(AgUiMessage::Assistant {
                        id,
                        content: Some(visible),
                        name: None,
                        tool_calls: None,
                    });
                }
                for tool_result in &tool_calls.tool_results {
                    let content = tool_result
                        .output
                        .as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| tool_result.output.to_string());
                    messages.push(AgUiMessage::Tool {
                        id: MessageId::random(),
                        content,
                        tool_call_id: serde_json::from_value(serde_json::Value::String(
                            tool_result
                                .call
                                .id
                                .clone()
                                .unwrap_or_else(|| "tool-call".to_string()),
                        ))
                        .expect("tool call id should deserialize from string"),
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
