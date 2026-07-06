#![allow(dead_code)]

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::session_actor::{
    PromptResult, SessionCommand, SessionHandle, SessionInfo, SessionRegistry, SubscribeResult,
};

#[cfg(test)]
use ag_ui_core::{event::RunStartedEvent, types::ids::RunId};
use ag_ui_core::{
    event::{
        BaseEvent, CustomEvent, Event, MessagesSnapshotEvent, RunErrorEvent, StepFinishedEvent,
        StepStartedEvent, TextMessageContentEvent, TextMessageStartEvent, ThinkingEndEvent,
        ThinkingStartEvent, ThinkingTextMessageContentEvent, ThinkingTextMessageEndEvent,
        ThinkingTextMessageStartEvent, ToolCallArgsEvent, ToolCallEndEvent, ToolCallResultEvent,
        ToolCallStartEvent,
    },
    types::{
        ids::{MessageId, ThreadId, ToolCallId},
        input::RunAgentInput,
        message::{Message as AgUiMessage, Role},
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
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use uuid::Uuid;

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
            AgentEvent::Status(_status) => {}
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
    tokio_stream::wrappers::IntervalStream::new({
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker
    })
    .skip(1)
    .map(|_| Bytes::from_static(keep_alive_frame().as_bytes()))
}

pub fn parse_run_input(body: &[u8]) -> Result<RunAgentInput<JsonValue, JsonValue>, AgUiError> {
    let mut value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|err| AgUiError::BadRequest(format!("invalid AG-UI JSON body: {err}")))?;

    let obj = value
        .as_object_mut()
        .ok_or_else(|| AgUiError::BadRequest("AG-UI body must be a JSON object".to_string()))?;

    if !obj.contains_key("messages") {
        return Err(AgUiError::BadRequest(
            "AG-UI request must include a messages field".to_string(),
        ));
    }

    obj.entry("state".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    obj.entry("tools".to_string())
        .or_insert_with(|| serde_json::Value::Array(vec![]));
    obj.entry("context".to_string())
        .or_insert_with(|| serde_json::Value::Array(vec![]));
    obj.entry("forwardedProps".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    serde_json::from_value(value)
        .map_err(|err| AgUiError::BadRequest(format!("invalid AG-UI request body: {err}")))
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

pub async fn ag_ui_run_with_call_fn(
    _base_config: &Config,
    registry: &SessionRegistry,
    agent: &str,
    session: &str,
    req_body: &[u8],
    _call_fn: Option<AgentCallFn>,
) -> Result<AppResponse, AgUiError> {
    let run_input = parse_run_input(req_body)?;
    let key = crate::session_actor::SessionKey {
        agent: agent.to_string(),
        session: session.to_string(),
    };
    let handle = registry.get_or_spawn(key);
    let SubscribeResult { snapshot, events } = subscribe(&handle).await;

    if let Some(text) = last_user_prompt(&run_input) {
        let _ = prompt(&handle, &text).await;
    }

    let thread_id = derive_thread_id(session);
    let snapshot_stream = tokio_stream::once(snapshot_event(snapshot));
    let handle_for_lag = handle.clone();
    let live_stream = BroadcastStream::new(events)
        .then(move |item| {
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
        })
        .filter_map(|event| event);

    let unsubscribe_guard = UnsubscribeOnDrop {
        handle: handle.clone(),
    };
    let event_stream = snapshot_stream.chain(live_stream).map(|event| {
        let frame = frame_event(&event).expect("AG-UI event framing should serialize");
        Bytes::from(frame)
    });
    let keep_alive_stream = keep_alive_stream(SSE_KEEPALIVE_INTERVAL);
    let stream = futures_util::stream::select(event_stream, keep_alive_stream).map(move |frame| {
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
mod tests {
    use super::*;
    use crate::test_support::TestConfigSandbox;
    use anyhow::anyhow;
    use harnx_core::{
        api_types::CompletionTokenUsage,
        event::{AgentEventSink, ModelEvent, SessionEvent, TurnEvent},
        message::MessageContentToolCalls,
    };
    use harnx_runtime::{client::ToolCall, config::Config};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};

    #[test]
    fn ag_ui_sink_emits_text_message_content_for_chunk() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id =
            MessageId::from(uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap());
        let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".to_string())],
            }),
            None,
        );

        match rx.try_recv().expect("text start") {
            Event::TextMessageStart(event) => {
                assert_eq!(event.message_id, message_id.clone());
                assert_eq!(event.role, Role::Assistant);
            }
            other => panic!("expected TextMessageStart event, got: {other:?}"),
        }
        match rx.try_recv().expect("text content") {
            Event::TextMessageContent(TextMessageContentEvent {
                base,
                message_id: mid,
                delta,
            }) => {
                assert_eq!(base.timestamp, None);
                assert_eq!(base.raw_event, None);
                assert_eq!(mid, message_id);
                assert_eq!(delta, "hello");
            }
            other => panic!("expected TextMessageContent event, got: {other:?}"),
        }

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_skips_empty_chunk() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::new(tx, message_id);

        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk { blocks: vec![] }),
            None,
        );
        assert!(rx.try_recv().is_err());

        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Image {
                    data: vec![],
                    mime: "image/png".to_string(),
                }],
            }),
            None,
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_emits_run_error_for_model_error() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::new(tx, message_id);

        sink.emit(
            AgentEvent::Model(ModelEvent::Error("boom".to_string())),
            None,
        );

        let event = rx.try_recv().expect("should receive event");
        match event {
            Event::RunError(RunErrorEvent {
                base,
                message,
                code,
            }) => {
                assert_eq!(base.timestamp, None);
                assert_eq!(base.raw_event, None);
                assert_eq!(message, "boom");
                assert!(code.is_none());
            }
            _ => panic!("expected RunError event, got: {:?}", event),
        }
    }

    #[test]
    fn ag_ui_sink_emits_run_error_for_notice_error() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::new(tx, message_id);

        sink.emit(
            AgentEvent::Notice(NoticeEvent::Error("fatal error".to_string())),
            None,
        );

        let event = rx.try_recv().expect("should receive event");
        match event {
            Event::RunError(RunErrorEvent { message, .. }) => {
                assert_eq!(message, "fatal error");
            }
            _ => panic!("expected RunError event, got: {:?}", event),
        }
    }

    #[test]
    fn ag_ui_sink_emits_final_as_content_when_non_empty() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

        let usage = CompletionTokenUsage::new(Some(10), Some(5), Some(0));
        sink.emit(
            AgentEvent::Model(ModelEvent::Final {
                output: "final text".to_string(),
                usage,
            }),
            None,
        );

        match rx.try_recv().expect("text start") {
            Event::TextMessageStart(event) => {
                assert_eq!(event.message_id, message_id.clone());
                assert_eq!(event.role, Role::Assistant);
            }
            other => panic!("expected TextMessageStart event, got: {other:?}"),
        }
        match rx.try_recv().expect("text content") {
            Event::TextMessageContent(TextMessageContentEvent {
                base,
                message_id: mid,
                delta,
            }) => {
                assert_eq!(mid, message_id);
                assert_eq!(delta, "final text");
                assert_eq!(base.timestamp, None);
            }
            other => panic!("expected TextMessageContent event, got: {other:?}"),
        }
    }

    #[test]
    fn ag_ui_sink_skips_final_when_empty() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::new(tx, message_id);

        let usage = CompletionTokenUsage::new(Some(10), Some(5), Some(0));
        sink.emit(
            AgentEvent::Model(ModelEvent::Final {
                output: String::new(),
                usage,
            }),
            None,
        );

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_maps_thought_then_text_and_closes_thinking_before_text() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

        sink.emit(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text("thinking...".to_string())],
            }),
            None,
        );
        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("answer".to_string())],
            }),
            None,
        );
        sink.emit(
            AgentEvent::Model(ModelEvent::Final {
                output: String::new(),
                usage: CompletionTokenUsage::default(),
            }),
            None,
        );

        assert!(matches!(
            rx.try_recv().expect("thinking start"),
            Event::ThinkingStart(_)
        ));
        match rx.try_recv().expect("thinking text start") {
            Event::ThinkingTextMessageStart(event) => assert_eq!(event.base.timestamp, None),
            other => panic!("expected ThinkingTextMessageStart, got: {other:?}"),
        }
        match rx.try_recv().expect("thinking delta") {
            Event::ThinkingTextMessageContent(event) => assert_eq!(event.delta, "thinking..."),
            other => panic!("expected ThinkingTextMessageContent, got: {other:?}"),
        }
        assert!(matches!(
            rx.try_recv().expect("thinking end"),
            Event::ThinkingTextMessageEnd(_)
        ));
        assert!(matches!(
            rx.try_recv().expect("thinking close"),
            Event::ThinkingEnd(_)
        ));
        match rx.try_recv().expect("text start") {
            Event::TextMessageStart(event) => {
                assert_eq!(event.message_id, message_id);
                assert_eq!(event.role, Role::Assistant);
            }
            other => panic!("expected TextMessageStart, got: {other:?}"),
        }
        match rx.try_recv().expect("text delta") {
            Event::TextMessageContent(event) => {
                assert_eq!(event.message_id, message_id);
                assert_eq!(event.delta, "answer");
            }
            other => panic!("expected TextMessageContent, got: {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_closes_thinking_before_tool_events() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);

        sink.emit(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text("planning".to_string())],
            }),
            None,
        );
        sink.emit(
            AgentEvent::Tool(ToolEvent::Started {
                id: "tool-1".to_string(),
                name: "read_history".to_string(),
                kind: harnx_core::event::ToolKind::Other,
                markdown: None,
                input: json!({"limit": 1}),
                locations: vec![],
            }),
            None,
        );

        assert!(matches!(
            rx.try_recv().expect("thinking start"),
            Event::ThinkingStart(_)
        ));
        assert!(matches!(
            rx.try_recv().expect("thinking text start"),
            Event::ThinkingTextMessageStart(_)
        ));
        match rx.try_recv().expect("thinking delta") {
            Event::ThinkingTextMessageContent(event) => assert_eq!(event.delta, "planning"),
            other => panic!("expected ThinkingTextMessageContent, got: {other:?}"),
        }
        assert!(matches!(
            rx.try_recv().expect("thinking text end"),
            Event::ThinkingTextMessageEnd(_)
        ));
        assert!(matches!(
            rx.try_recv().expect("thinking end"),
            Event::ThinkingEnd(_)
        ));
        match rx.try_recv().expect("tool start") {
            Event::ToolCallStart(event) => {
                assert_eq!(event.parent_message_id, Some(message_id));
                assert_eq!(event.tool_call_name, "read_history");
            }
            other => panic!("expected ToolCallStart, got: {other:?}"),
        }
        assert!(matches!(
            rx.try_recv().expect("tool args"),
            Event::ToolCallArgs(_)
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_keeps_thinking_open_across_background_session_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::with_snapshot(tx, message_id, false, None);

        sink.emit(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text("thinking one".to_string())],
            }),
            None,
        );
        sink.emit(
            AgentEvent::Session(SessionEvent::Saved {
                path: "/tmp/session.json".into(),
            }),
            None,
        );
        sink.emit(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text(" thinking two".to_string())],
            }),
            None,
        );
        sink.emit(
            AgentEvent::Turn(TurnEvent::Ended {
                outcome: harnx_core::event::TurnOutcome::default(),
            }),
            None,
        );

        assert!(matches!(
            rx.try_recv().expect("thinking start"),
            Event::ThinkingStart(_)
        ));
        assert!(matches!(
            rx.try_recv().expect("thinking text start"),
            Event::ThinkingTextMessageStart(_)
        ));
        match rx.try_recv().expect("first thinking delta") {
            Event::ThinkingTextMessageContent(event) => assert_eq!(event.delta, "thinking one"),
            other => panic!("expected ThinkingTextMessageContent, got: {other:?}"),
        }
        match rx.try_recv().expect("saved custom event") {
            Event::Custom(event) => {
                assert_eq!(event.name, "session_saved");
                assert_eq!(event.value, json!({ "path": "/tmp/session.json" }));
            }
            other => panic!("expected Custom, got: {other:?}"),
        }
        match rx.try_recv().expect("second thinking delta") {
            Event::ThinkingTextMessageContent(event) => assert_eq!(event.delta, " thinking two"),
            other => panic!("expected ThinkingTextMessageContent, got: {other:?}"),
        }
        assert!(matches!(
            rx.try_recv().expect("thinking text end"),
            Event::ThinkingTextMessageEnd(_)
        ));
        assert!(matches!(
            rx.try_recv().expect("thinking end"),
            Event::ThinkingEnd(_)
        ));
        assert!(matches!(
            rx.try_recv().expect("step finished"),
            Event::StepFinished(_)
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_maps_tool_started_completed_to_ag_ui_events() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::with_snapshot(tx, message_id.clone(), false, None);
        sink.emit(
            AgentEvent::Tool(ToolEvent::Started {
                id: "history-1".to_string(),
                name: "read_history".to_string(),
                kind: harnx_core::event::ToolKind::Other,
                markdown: None,
                input: json!({"limit": 5, "entry_type": "message"}),
                locations: vec![],
            }),
            None,
        );
        sink.emit(
            AgentEvent::Tool(ToolEvent::Completed {
                id: "history-1".to_string(),
                output: json!("history checked"),
                markdown: None,
            }),
            None,
        );

        match rx.try_recv().expect("tool start") {
            Event::ToolCallStart(event) => {
                assert_eq!(
                    event.tool_call_id,
                    serde_json::from_value(json!("history-1")).unwrap()
                );
                assert_eq!(event.tool_call_name, "read_history");
                assert_eq!(event.parent_message_id, Some(message_id.clone()));
            }
            other => panic!("expected ToolCallStart, got: {other:?}"),
        }

        match rx.try_recv().expect("tool args") {
            Event::ToolCallArgs(event) => {
                assert_eq!(
                    event.tool_call_id,
                    serde_json::from_value(json!("history-1")).unwrap()
                );
                assert_eq!(
                    event.delta,
                    json!({"limit": 5, "entry_type": "message"}).to_string()
                );
            }
            other => panic!("expected ToolCallArgs, got: {other:?}"),
        }

        match rx.try_recv().expect("tool end") {
            Event::ToolCallEnd(event) => {
                assert_eq!(
                    event.tool_call_id,
                    serde_json::from_value(json!("history-1")).unwrap()
                );
            }
            other => panic!("expected ToolCallEnd, got: {other:?}"),
        }

        match rx.try_recv().expect("tool result") {
            Event::ToolCallResult(event) => {
                assert_eq!(
                    event.tool_call_id,
                    serde_json::from_value(json!("history-1")).unwrap()
                );
                assert_eq!(event.content, "history checked");
                assert_eq!(event.role, Role::Tool);
            }
            other => panic!("expected ToolCallResult, got: {other:?}"),
        }

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_maps_tool_failures_and_blocked_to_results() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::new(tx, message_id);

        sink.emit(
            AgentEvent::Tool(ToolEvent::Failed {
                id: "tool-fail".to_string(),
                error: "boom".to_string(),
            }),
            None,
        );
        sink.emit(
            AgentEvent::Tool(ToolEvent::Blocked {
                id: "tool-blocked".to_string(),
                name: "danger".to_string(),
                input: json!({"path": "/tmp"}),
                reason: "blocked by policy".to_string(),
            }),
            None,
        );

        for (expected_id, expected_content) in
            [("tool-fail", "boom"), ("tool-blocked", "blocked by policy")]
        {
            match rx.try_recv().expect("tool end") {
                Event::ToolCallEnd(event) => {
                    assert_eq!(
                        event.tool_call_id,
                        serde_json::from_value(json!(expected_id)).unwrap()
                    );
                }
                other => panic!("expected ToolCallEnd, got: {other:?}"),
            }
            match rx.try_recv().expect("tool result") {
                Event::ToolCallResult(event) => {
                    assert_eq!(
                        event.tool_call_id,
                        serde_json::from_value(json!(expected_id)).unwrap()
                    );
                    assert_eq!(event.content, expected_content);
                    assert_eq!(event.role, Role::Tool);
                }
                other => panic!("expected ToolCallResult, got: {other:?}"),
            }
        }

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn frame_event_uses_data_only_sse_format() {
        let event: Event = Event::RunStarted(RunStartedEvent {
            base: BaseEvent {
                timestamp: None,
                raw_event: None,
            },
            thread_id: ThreadId::from(
                Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap(),
            ),
            run_id: RunId::from(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap()),
        });

        let framed = frame_event(&event).expect("frame should serialize");
        assert!(framed.starts_with("data: {\"type\":\"RUN_STARTED\""));
        assert!(framed.ends_with("\n\n"));
        assert!(!framed.contains("event:"));
    }

    #[test]
    fn parse_run_input_accepts_valid_body() {
        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "state": {},
            "messages": [{
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "hello"
            }],
            "tools": [],
            "context": [],
            "forwardedProps": {}
        });

        let parsed = parse_run_input(&serde_json::to_vec(&body).unwrap()).expect("valid body");
        assert_eq!(parsed.messages.len(), 1);
    }

    #[test]
    fn ag_ui_sink_maps_turn_started_and_ended_to_step_events() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let sink = super::AgUiSink::with_snapshot(tx, MessageId::random(), true, None);

        sink.emit(AgentEvent::Turn(TurnEvent::Started), None);
        sink.emit(
            AgentEvent::Turn(TurnEvent::Ended {
                outcome: harnx_core::event::TurnOutcome::default(),
            }),
            None,
        );

        match rx.try_recv().expect("step started") {
            Event::StepStarted(event) => assert_eq!(event.step_name, "turn-1"),
            other => panic!("expected StepStarted, got: {other:?}"),
        }
        match rx.try_recv().expect("step finished") {
            Event::StepFinished(event) => assert_eq!(event.step_name, "turn-1"),
            other => panic!("expected StepFinished, got: {other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_maps_plan_event_to_custom() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let sink = super::AgUiSink::with_snapshot(tx, MessageId::random(), true, None);

        sink.emit(
            AgentEvent::Plan {
                entries: vec![harnx_core::event::PlanEntry {
                    status: "in_progress".to_string(),
                    content: "check logs".to_string(),
                }],
            },
            None,
        );

        match rx.try_recv().expect("plan custom") {
            Event::Custom(event) => {
                assert_eq!(event.name, "plan");
                assert_eq!(
                    event.value,
                    json!([{"status": "in_progress", "content": "check logs"}])
                );
            }
            other => panic!("expected Custom plan, got: {other:?}"),
        }
    }

    #[test]
    fn ag_ui_sink_maps_compaction_completed_to_custom_and_snapshot() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let snapshot = Arc::new(|| {
            vec![AgUiMessage::User {
                id: MessageId::random(),
                content: "compacted".to_string(),
                name: None,
            }]
        });
        let sink = super::AgUiSink::with_snapshot(tx, MessageId::random(), true, Some(snapshot));

        sink.emit(AgentEvent::Session(SessionEvent::CompactingCompleted), None);

        match rx.try_recv().expect("compaction custom") {
            Event::Custom(event) => {
                assert_eq!(event.name, "session_compacting_completed");
                assert_eq!(event.value, json!({}));
            }
            other => panic!("expected compaction custom, got: {other:?}"),
        }
        match rx.try_recv().expect("snapshot") {
            Event::MessagesSnapshot(event) => {
                assert_eq!(event.messages.len(), 1);
                assert!(
                    matches!(&event.messages[0], AgUiMessage::User { content, .. } if content == "compacted")
                );
            }
            other => panic!("expected MessagesSnapshot, got: {other:?}"),
        }
    }

    #[test]
    fn parse_run_input_accepts_minimal_body_without_optional_fields() {
        // Per plan guardrail: only `messages` is required; state/tools/context/forwardedProps
        // are semantically optional and should default if omitted.
        let thread_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let body = json!({
            "threadId": thread_id,
            "runId": run_id,
            "messages": [{
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "hello"
            }]
        });

        let parsed = parse_run_input(&serde_json::to_vec(&body).unwrap()).expect("minimal body");
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.thread_id, ThreadId::from(thread_id));
        assert_eq!(parsed.run_id, RunId::from(run_id));
    }

    #[test]
    fn parse_run_input_rejects_missing_messages_field() {
        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "state": {},
            "tools": [],
            "context": [],
            "forwardedProps": {}
        });

        let err = parse_run_input(&serde_json::to_vec(&body).unwrap())
            .expect_err("missing messages should fail");
        assert!(matches!(err, AgUiError::BadRequest(msg) if msg.contains("messages")));
    }

    #[test]
    fn parse_run_input_accepts_empty_messages_for_join_resume() {
        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "state": {},
            "messages": [],
            "tools": [],
            "context": [],
            "forwardedProps": {}
        });

        let parsed = parse_run_input(&serde_json::to_vec(&body).unwrap())
            .expect("empty messages should be allowed for join/resume");
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn resolve_agent_finds_existing_agent() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("hephaestus", "You are Hephaestus.");
        let config = sandbox.config();

        let agent = resolve_agent(&config, "hephaestus").expect("agent should resolve");
        assert_eq!(agent.name(), "hephaestus");
    }

    #[test]
    fn resolve_agent_returns_not_found_for_unknown_agent() {
        let sandbox = TestConfigSandbox::new();
        let config = sandbox.config();

        let err = resolve_agent(&config, "missing-agent").expect_err("missing agent should fail");
        assert_eq!(
            err,
            AgUiError::NotFound("agent 'missing-agent' not found".to_string())
        );
    }

    #[test]
    fn derive_thread_id_is_deterministic_and_passes_through_uuid() {
        let non_uuid = "not-a-uuid-session";
        let derived_a = derive_thread_id(non_uuid);
        let derived_b = derive_thread_id(non_uuid);
        assert_eq!(derived_a, derived_b);

        let session_uuid = Uuid::new_v4();
        let thread_id = derive_thread_id(&session_uuid.to_string());
        assert_eq!(thread_id, ThreadId::from(session_uuid));
    }

    #[test]
    fn build_local_input_sets_agent_and_session_meta() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("hephaestus", "You are Hephaestus.");
        let base_config = sandbox.config();
        let prompt_config = harnx_session::fork_prompt_config(&base_config);
        {
            let mut config = prompt_config.write();
            config
                .use_agent_by_name("hephaestus")
                .expect("agent should load");
            config
                .use_session(Some("session-a"))
                .expect("session should load");
        }

        let input = build_local_input(&prompt_config, "hephaestus", "session-a", "hello world")
            .expect("input should build");
        assert!(input.with_session());
        assert!(input.with_agent());

        let session = prompt_config
            .read()
            .session
            .clone()
            .expect("session should exist");
        assert_eq!(session.agent_name.as_deref(), Some("hephaestus"));
    }

    #[tokio::test]
    async fn ag_ui_run_streams_ordered_events_with_stubbed_call_fn() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("hephaestus", "You are Hephaestus.");
        let config = sandbox.config();

        let run_id_uuid = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let session_id = "run-session-1";
        let req_body = serde_json::to_vec(&json!({
            "threadId": Uuid::new_v4(),
            "runId": run_id_uuid,
            "state": {},
            "messages": [{
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "forge response"
            }],
            "tools": [],
            "context": [],
            "forwardedProps": {}
        }))
        .unwrap();

        let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async move {
                harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text("chunk-text".to_string())],
                }));
                Ok((
                    "assistant final".to_string(),
                    None,
                    Vec::<ToolCall>::new(),
                    CompletionTokenUsage::default(),
                ))
            })
        });

        let registry = crate::session_actor::SessionRegistry::new_for_tests(
            config.clone(),
            std::time::Duration::from_secs(30),
            Some(call_fn),
        );
        let response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "hephaestus",
            session_id,
            &req_body,
            None,
        )
        .await
        .expect("AG-UI run should succeed");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );

        let parsed_events = read_sse_events_until(response, |events| {
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
        })
        .await;

        let event_types = parsed_events
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            vec![
                "MESSAGES_SNAPSHOT",
                "RUN_STARTED",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "RUN_FINISHED",
            ]
        );

        assert_eq!(parsed_events[3]["delta"].as_str(), Some("chunk-text"));

        let start_message_id = parsed_events[2]["messageId"].as_str().unwrap().to_string();
        assert_eq!(
            parsed_events[3]["messageId"].as_str(),
            Some(start_message_id.as_str())
        );
        assert_eq!(
            parsed_events[3]["messageId"].as_str(),
            Some(start_message_id.as_str())
        );

        let session_messages = load_session_texts(&config, "hephaestus", session_id);
        assert!(
            session_messages.iter().any(|text| text == "forge response"),
            "user prompt should persist via loop"
        );
        assert!(
            session_messages
                .iter()
                .any(|text| text == "assistant final"),
            "assistant output should persist via loop"
        );
    }

    #[tokio::test]
    async fn ag_ui_run_emits_run_error_without_run_finished_when_call_fn_fails() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "error-path-session";
        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [{
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "fail deterministically"
            }]
        });

        let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async { Err(anyhow!("stubbed call failure")) })
        });

        let registry = crate::session_actor::SessionRegistry::new_for_tests(
            config.clone(),
            std::time::Duration::from_secs(30),
            Some(call_fn),
        );
        let response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&body).unwrap(),
            None,
        )
        .await
        .expect("ag ui response");
        let parsed_events = read_sse_events_until(response, |events| {
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_ERROR"))
        })
        .await;
        let event_types = parsed_events
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            vec![
                "MESSAGES_SNAPSHOT",
                "RUN_STARTED",
                "TEXT_MESSAGE_START",
                "RUN_ERROR",
            ]
        );
        assert_eq!(
            parsed_events[3]["message"].as_str(),
            Some("stubbed call failure")
        );
        assert!(parsed_events
            .iter()
            .all(|event| event["type"].as_str() != Some("RUN_FINISHED")));
    }

    #[tokio::test]
    async fn ag_ui_run_lists_persisted_session_and_history_for_agent() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "enumeration-session";
        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [{
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "enumerate persisted history"
            }]
        });

        let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async {
                let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
                Ok(("persisted assistant".into(), None, vec![], usage))
            })
        });

        let registry = crate::session_actor::SessionRegistry::new_for_tests(
            config.clone(),
            std::time::Duration::from_secs(30),
            Some(call_fn),
        );
        let response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&body).unwrap(),
            None,
        )
        .await
        .expect("persisted run response");
        assert_run_finished(
            read_sse_events_until(response, |events| {
                events
                    .iter()
                    .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
            })
            .await,
        );

        let scoped = super::super::agent_scoped_config(&config, "plain").expect("scoped config");
        let sessions = super::super::agent_sessions_json(&config, "plain").expect("session list");
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].get("session_id").is_some());

        let loaded = harnx_runtime::config::session::load(
            &scoped,
            session_id,
            &scoped.session_file(session_id),
        )
        .expect("load persisted session");
        assert_eq!(loaded.agent_name.as_deref(), Some("plain"));

        let mut history_iter = loaded.messages.iter();
        let persisted_user = history_iter
            .find(|message| message.role.is_user())
            .expect("persisted user message");
        let persisted_assistant = history_iter
            .find(|message| message.role.is_assistant())
            .expect("persisted assistant message");
        assert_eq!(
            persisted_user.content.to_text(),
            "enumerate persisted history"
        );
        assert_eq!(persisted_assistant.content.to_text(), "persisted assistant");

        let shaped = loaded
            .messages
            .iter()
            .filter(|message| message.role.is_user() || message.role.is_assistant())
            .map(|message| {
                json!({
                    "id": format!(
                        "seq:{}:0",
                        message.log_seq.expect("persisted history should have log seq")
                    ),
                    "role": super::super::history_role_name(message.role),
                    "content": super::super::history_message_content(message),
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shaped,
            vec![
                json!({
                    "id": persisted_user
                        .log_seq
                        .map(|seq| format!("seq:{seq}:0"))
                        .expect("persisted user should have log seq"),
                    "role": "user",
                    "content": "enumerate persisted history"
                }),
                json!({
                    "id": persisted_assistant
                        .log_seq
                        .map(|seq| format!("seq:{seq}:0"))
                        .expect("persisted assistant should have log seq"),
                    "role": "assistant",
                    "content": "persisted assistant"
                }),
            ]
        );

        let all_agents = Config::all_agents();
        assert!(all_agents.iter().any(|agent| agent.name() == "plain"));
    }

    #[tokio::test]
    async fn ag_ui_run_uses_same_message_id_for_wire_and_persistence() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "wire-id-session";
        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [{
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "user1"
            }]
        });

        let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async {
                let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
                Ok(("assistant1".into(), None, vec![], usage))
            })
        });

        let registry = crate::session_actor::SessionRegistry::new_for_tests(
            config.clone(),
            std::time::Duration::from_secs(30),
            Some(call_fn),
        );
        let response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&body).unwrap(),
            None,
        )
        .await
        .expect("run response");
        let events = read_sse_events_until(response, |events| {
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
        })
        .await;
        assert_run_finished(events.clone());

        let wire_message_ids = events
            .iter()
            .filter_map(|event| match event["type"].as_str() {
                Some("TEXT_MESSAGE_START")
                | Some("TEXT_MESSAGE_CONTENT")
                | Some("TEXT_MESSAGE_END") => event["messageId"].as_str().map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !wire_message_ids.is_empty(),
            "expected wire message ids in SSE: {events:?}"
        );
        assert!(
            wire_message_ids.windows(2).all(|ids| ids[0] == ids[1]),
            "wire message ids should stay stable across SSE events: {wire_message_ids:?}"
        );
        let persisted_messages = load_session_messages(&config, "plain", session_id);
        let persisted_assistant = persisted_messages
            .iter()
            .find(|msg| msg.role.is_assistant())
            .expect("persisted assistant message");
        assert!(persisted_assistant.id.is_some());
    }

    fn assert_bad_request_contains(err: &AgUiError, expected: &str) {
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        match err {
            AgUiError::BadRequest(message) => {
                assert!(
                    message.contains(expected),
                    "expected bad request to contain {expected:?}, got {message:?}"
                );
            }
            other => panic!("expected bad request error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ag_ui_run_idle_join_emits_keep_alive_comment() {
        tokio::time::pause();

        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "idle-keepalive-session";
        let registry = crate::session_actor::SessionRegistry::new(config.clone());
        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": []
        });

        let response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&body).unwrap(),
            None,
        )
        .await
        .expect("idle join response");

        let read_task = tokio::spawn(async move {
            read_sse_until(
                response,
                SSE_KEEPALIVE_INTERVAL + Duration::from_secs(5),
                |read| !read.events.is_empty() && !read.comments.is_empty(),
            )
            .await
        });

        let mut frame_stream = tokio_stream::once(snapshot_event(Vec::new()))
            .map(|event| {
                let frame = frame_event(&event).expect("snapshot frame");
                Ok::<_, Infallible>(Bytes::from(frame))
            })
            .chain(keep_alive_stream(TEST_SSE_KEEPALIVE_INTERVAL).map(Ok::<_, Infallible>));

        let snapshot = tokio::time::timeout(Duration::from_secs(1), frame_stream.next())
            .await
            .expect("snapshot timeout")
            .expect("snapshot frame")
            .expect("snapshot bytes");
        let keep_alive = tokio::time::timeout(Duration::from_secs(1), frame_stream.next())
            .await
            .expect("keep-alive timeout")
            .expect("keep-alive frame")
            .expect("keep-alive bytes");

        let snapshot_text = std::str::from_utf8(&snapshot).expect("snapshot utf8");
        let keep_alive_text = std::str::from_utf8(&keep_alive).expect("keep-alive utf8");
        assert!(snapshot_text.starts_with("data: "));
        assert_eq!(keep_alive_text, keep_alive_frame());

        tokio::task::yield_now().await;
        tokio::time::advance(SSE_KEEPALIVE_INTERVAL + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        let read = read_task.await.expect("join read task");
        assert_eq!(read.events[0]["type"], "MESSAGES_SNAPSHOT");
        assert!(read.comments.iter().any(|frame| frame == ": keep-alive"));
        assert!(
            !read
                .events
                .iter()
                .any(|event| event["type"] == ": keep-alive"),
            "keep-alive comment should not parse as AG-UI event: {:?}",
            read.events
        );
    }

    #[tokio::test]
    async fn ag_ui_run_empty_messages_join_only_snapshot_no_new_run() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "join-only-session";

        let first_body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [{
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "user1"
            }]
        });
        let first_call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async {
                let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
                Ok(("assistant1".into(), None, vec![], usage))
            })
        });
        let registry = crate::session_actor::SessionRegistry::new_for_tests(
            config.clone(),
            std::time::Duration::from_secs(30),
            Some(first_call_fn),
        );
        let first_response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&first_body).unwrap(),
            None,
        )
        .await
        .expect("first run");
        let _ = read_sse_events_until(first_response, |events| {
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
        })
        .await;

        let join_body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": []
        });
        let response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&join_body).unwrap(),
            None,
        )
        .await
        .expect("join only");
        let events = read_sse_events_until(response, |events| !events.is_empty()).await;
        assert_eq!(events[0]["type"], "MESSAGES_SNAPSHOT");
        assert!(!events.iter().any(|event| event["type"] == "RUN_STARTED"));
    }

    #[tokio::test]
    async fn ag_ui_run_uses_only_last_message_user_prompt() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "last-message-session";

        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [
                {
                    "id": Uuid::new_v4(),
                    "role": "user",
                    "content": "ignored earlier"
                },
                {
                    "id": Uuid::new_v4(),
                    "role": "assistant",
                    "content": "assistant history"
                },
                {
                    "id": Uuid::new_v4(),
                    "role": "user",
                    "content": "last user wins"
                }
            ]
        });
        let call_fn: AgentCallFn = Arc::new(|input, _config, _abort| {
            let text = input.text();
            Box::pin(async move {
                assert_eq!(text, "last user wins");
                let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
                Ok(("assistant2".into(), None, vec![], usage))
            })
        });
        let registry = crate::session_actor::SessionRegistry::new_for_tests(
            config.clone(),
            std::time::Duration::from_secs(30),
            Some(call_fn),
        );
        let response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&body).unwrap(),
            None,
        )
        .await
        .expect("last message prompt run");
        let events = read_sse_events_until(response, |events| {
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
        })
        .await;
        assert_eq!(events[0]["type"], "MESSAGES_SNAPSHOT");
        assert!(events.iter().any(|event| event["type"] == "RUN_STARTED"));
    }

    #[tokio::test]
    async fn ag_ui_run_resume_same_session_persists_only_new_turn() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "resume-session";

        let first_body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [{
                "id": Uuid::new_v4(),
                "role": "user",
                "content": "user1"
            }]
        });
        let first_call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async {
                let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
                Ok(("assistant1".into(), None, vec![], usage))
            })
        });
        let registry = crate::session_actor::SessionRegistry::new_for_tests(
            config.clone(),
            std::time::Duration::from_secs(30),
            Some(first_call_fn),
        );
        let first_response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&first_body).unwrap(),
            None,
        )
        .await
        .expect("first run");
        assert_run_finished(
            read_sse_events_until(first_response, |events| {
                events
                    .iter()
                    .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
            })
            .await,
        );
        let first_messages = load_session_texts(&config, "plain", session_id);
        assert_eq!(
            first_messages,
            vec!["user1".to_string(), "assistant1".to_string()]
        );

        let persisted_messages = load_session_messages(&config, "plain", session_id);
        let persisted_assistant_id = persisted_messages
            .iter()
            .find(|msg| msg.role.is_assistant())
            .and_then(|msg| msg.id.clone())
            .expect("persisted assistant id");

        let second_body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [
                {
                    "id": MessageId::random(),
                    "role": "user",
                    "content": "user1"
                },
                {
                    "id": persisted_assistant_id,
                    "role": "assistant",
                    "content": "assistant1"
                },
                {
                    "id": MessageId::random(),
                    "role": "user",
                    "content": "user2"
                }
            ]
        });
        let second_call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async {
                let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
                Ok(("assistant2".into(), None, vec![], usage))
            })
        });
        let registry = crate::session_actor::SessionRegistry::new_for_tests(
            config.clone(),
            std::time::Duration::from_secs(30),
            Some(second_call_fn),
        );
        let second_response = ag_ui_run_with_call_fn(
            &config,
            &registry,
            "plain",
            session_id,
            &serde_json::to_vec(&second_body).unwrap(),
            None,
        )
        .await
        .expect("second run");
        assert_run_finished(
            read_sse_events_until(second_response, |events| {
                events
                    .iter()
                    .any(|event| event["type"].as_str() == Some("RUN_FINISHED"))
            })
            .await,
        );
        let second_messages = load_session_texts(&config, "plain", session_id);
        assert_eq!(second_messages.len(), first_messages.len() + 2);
        assert_eq!(
            second_messages,
            vec![
                "user1".to_string(),
                "assistant1".to_string(),
                "user2".to_string(),
                "assistant2".to_string(),
            ]
        );
    }

    fn parse_sse_frames(body: &str) -> Vec<String> {
        body.split("\n\n")
            .filter_map(|frame| {
                let trimmed = frame.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect()
    }

    async fn parse_sse_events(response: AppResponse) -> Vec<Value> {
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect sse body")
            .to_bytes();
        let body_text = std::str::from_utf8(&body).expect("sse utf8");
        parse_sse_frames(body_text)
            .into_iter()
            .map(|frame| parse_sse_frame(&frame))
            .collect()
    }

    async fn read_sse_events_until<F>(response: AppResponse, done: F) -> Vec<Value>
    where
        F: Fn(&[Value]) -> bool,
    {
        read_sse_until(response, std::time::Duration::from_secs(5), |read| {
            done(&read.events)
        })
        .await
        .events
    }

    async fn read_sse_until<F>(response: AppResponse, timeout: Duration, done: F) -> SseRead
    where
        F: Fn(&SseRead) -> bool,
    {
        let mut body = response.into_body().into_data_stream();
        let mut read = SseRead::default();
        let mut partial = String::new();

        while !done(&read) {
            let next = tokio::time::timeout(timeout, body.next())
                .await
                .expect("timed out waiting for SSE chunk");
            let chunk = next
                .expect("sse stream ended before expected frame")
                .expect("stream chunk");
            partial.push_str(std::str::from_utf8(&chunk).expect("sse utf8"));

            while let Some(idx) = partial.find("\n\n") {
                let frame = partial[..idx].trim().to_string();
                partial.drain(..idx + 2);
                if frame.is_empty() {
                    continue;
                }
                read.frames.push(frame.clone());
                if frame.starts_with(':') {
                    read.comments.push(frame);
                } else {
                    read.events.push(parse_sse_frame(&frame));
                }
                if done(&read) {
                    return read;
                }
            }
        }

        read
    }

    #[derive(Debug, Default)]
    struct SseRead {
        frames: Vec<String>,
        events: Vec<Value>,
        comments: Vec<String>,
    }

    fn parse_sse_frame(frame: &str) -> Value {
        let payload = frame
            .strip_prefix("data: ")
            .expect("sse frame should start with data prefix");
        serde_json::from_str(payload).expect("frame should be valid json")
    }

    fn assert_run_finished(events: Vec<Value>) {
        assert!(
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("RUN_FINISHED")),
            "expected RUN_FINISHED barrier, got events: {events:?}"
        );
    }

    fn load_session_texts(config: &Config, agent: &str, session_id: &str) -> Vec<String> {
        let prompt_config = harnx_session::fork_prompt_config(config);
        {
            let mut cfg = prompt_config.write();
            cfg.use_agent_by_name(agent).expect("set agent");
            cfg.use_session(Some(session_id)).expect("load session");
        }
        let session_messages = prompt_config
            .read()
            .session
            .as_ref()
            .expect("session should exist")
            .messages
            .iter()
            .filter(|msg| msg.role.is_user() || msg.role.is_assistant())
            .map(|msg| msg.content.to_text())
            .collect();
        session_messages
    }
    fn load_session_messages(config: &Config, agent: &str, session_id: &str) -> Vec<HistoryMsg> {
        let prompt_config = harnx_session::fork_prompt_config(config);
        {
            let mut cfg = prompt_config.write();
            cfg.use_agent_by_name(agent).expect("set agent");
            cfg.use_session(Some(session_id)).expect("load session");
        }
        let messages = prompt_config
            .read()
            .session
            .as_ref()
            .expect("session should exist")
            .messages
            .iter()
            .filter(|msg| msg.role.is_user() || msg.role.is_assistant())
            .cloned()
            .collect();
        messages
    }

    #[test]
    fn history_messages_for_snapshot_keeps_tool_turn_prose_and_tool_entries() {
        let tool_call = harnx_core::tool::ToolCall::new(
            "history".into(),
            json!({"limit": 5}),
            Some("call-1".into()),
            None,
        );
        let history = vec![
            HistoryMsg {
                role: MessageRole::User,
                content: MessageContent::Text("prompt".to_string()),
                id: Some(Uuid::new_v4().to_string()),
                log_seq: None,
                log_timestamp: None,
            },
            HistoryMsg {
                role: MessageRole::Assistant,
                content: MessageContent::ToolCalls(MessageContentToolCalls::new(
                    vec![harnx_core::tool::ToolResult::new(
                        tool_call,
                        json!("tool output"),
                    )],
                    "assistant prose".to_string(),
                    None,
                )),
                id: Some(Uuid::new_v4().to_string()),
                log_seq: None,
                log_timestamp: None,
            },
        ];

        let snapshot = history_messages_for_snapshot(&history);
        assert_eq!(snapshot.len(), 3);
        assert!(matches!(&snapshot[0], AgUiMessage::User { content, .. } if content == "prompt"));
        assert!(
            matches!(&snapshot[1], AgUiMessage::Assistant { content, .. } if content.as_deref() == Some("assistant prose"))
        );
        assert!(
            matches!(&snapshot[2], AgUiMessage::Tool { content, .. } if content.contains("tool output"))
        );
    }

    fn user_msg(content: &str) -> AgUiMessage {
        AgUiMessage::User {
            id: MessageId::random(),
            content: content.to_string(),
            name: None,
        }
    }

    fn assistant_msg(content: &str) -> AgUiMessage {
        AgUiMessage::Assistant {
            id: MessageId::random(),
            content: Some(content.to_string()),
            name: None,
            tool_calls: None,
        }
    }
}
