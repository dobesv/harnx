#![allow(dead_code)]

use std::convert::Infallible;
use std::sync::Arc;

use ag_ui_core::{
    event::{
        BaseEvent, Event, RunErrorEvent, RunFinishedEvent, RunStartedEvent,
        TextMessageContentEvent, TextMessageEndEvent, TextMessageStartEvent,
    },
    types::{
        ids::{MessageId, RunId, ThreadId},
        input::RunAgentInput,
        message::{Message as AgUiMessage, Role},
    },
    JsonValue,
};
use bytes::Bytes;
use harnx_core::{
    abort::create_abort_signal,
    agent_config::AgentConfig,
    event::{AgentEvent, AgentSource, ContentBlock, ModelEvent, NoticeEvent},
    message::{Message as HistoryMsg, MessageContent, MessageRole},
    sink::with_agent_event_sink,
};
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use harnx_runtime::{
    config::{input, Agent, Config, GlobalConfig},
    run_agent_loop, AgentCallFn, AgentLoopContext,
};
use http::{Response, StatusCode};
use http_body_util::{combinators::BoxBody, BodyExt, StreamBody};
use hyper::body::Frame;
use parking_lot::RwLock;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use uuid::Uuid;

const THREAD_ID_NAMESPACE: Uuid = Uuid::from_u128(0x9f1f_5b4f_8080_4c1a_9544_1ce1_4b63_1a2f);

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
pub struct AgUiSink {
    tx: UnboundedSender<Event>,
    message_id: MessageId,
}

impl AgUiSink {
    pub fn new(tx: UnboundedSender<Event>, message_id: MessageId) -> Self {
        Self { tx, message_id }
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
                    let _ = self
                        .tx
                        .send(Event::TextMessageContent(TextMessageContentEvent {
                            base: BaseEvent {
                                timestamp: None,
                                raw_event: None,
                            },
                            message_id: self.message_id.clone(),
                            delta,
                        }));
                }
            }
            AgentEvent::Model(ModelEvent::Final { output, .. }) if !output.is_empty() => {
                let _ = self
                    .tx
                    .send(Event::TextMessageContent(TextMessageContentEvent {
                        base: BaseEvent {
                            timestamp: None,
                            raw_event: None,
                        },
                        message_id: self.message_id.clone(),
                        delta: output,
                    }));
            }
            AgentEvent::Model(ModelEvent::Error(message))
            | AgentEvent::Notice(NoticeEvent::Error(message)) => {
                let _ = self.tx.send(Event::RunError(RunErrorEvent {
                    base: BaseEvent {
                        timestamp: None,
                        raw_event: None,
                    },
                    message,
                    code: None,
                }));
            }
            AgentEvent::Model(ModelEvent::ThoughtChunk { .. }) => {}
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

pub fn parse_run_input(body: &[u8]) -> Result<RunAgentInput<JsonValue, JsonValue>, AgUiError> {
    // Parse into a generic JSON value first to inject defaults for optional envelope fields.
    // ag-ui-core's RunAgentInput requires state/tools/context/forwardedProps, but per plan
    // guardrails only `messages` is truly required — others should default if omitted.
    let mut value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|err| AgUiError::BadRequest(format!("invalid AG-UI request body: {err}")))?;

    // Inject defaults for optional envelope fields if missing.
    if let Some(obj) = value.as_object_mut() {
        if !obj.contains_key("state") {
            obj.insert("state".to_string(), serde_json::json!({}));
        }
        if !obj.contains_key("tools") {
            obj.insert("tools".to_string(), serde_json::json!([]));
        }
        if !obj.contains_key("context") {
            obj.insert("context".to_string(), serde_json::json!([]));
        }
        if !obj.contains_key("forwardedProps") {
            obj.insert("forwardedProps".to_string(), serde_json::json!({}));
        }
    }

    let input: RunAgentInput<JsonValue, JsonValue> = serde_json::from_value(value)
        .map_err(|err| AgUiError::BadRequest(format!("invalid AG-UI request body: {err}")))?;
    if input.messages.is_empty() {
        return Err(AgUiError::BadRequest(
            "AG-UI request must include at least one message".to_string(),
        ));
    }
    Ok(input)
}

pub fn resolve_agent(config: &Config, name: &str) -> Result<AgentConfig, AgUiError> {
    config
        .retrieve_agent(name)
        .map(Agent::into_config)
        .map_err(|_| AgUiError::NotFound(format!("agent '{name}' not found")))
}

pub fn reconcile_new_messages(
    persisted: &[HistoryMsg],
    client_msgs: &[AgUiMessage],
) -> Vec<NewMsg> {
    let matched = client_msgs
        .iter()
        .zip(persisted.iter())
        .take_while(|(client, history)| client_matches_history(client, history))
        .count();

    client_msgs[matched..]
        .iter()
        .filter_map(as_new_msg)
        .collect()
}
pub fn derive_thread_id(session_id: &str) -> ThreadId {
    let uuid = Uuid::parse_str(session_id)
        .unwrap_or_else(|_| Uuid::new_v5(&THREAD_ID_NAMESPACE, session_id.as_bytes()));
    ThreadId::from(uuid)
}

pub fn run_id_from_input(input: &RunAgentInput<JsonValue, JsonValue>) -> RunId {
    input.run_id.clone()
}

pub fn fork_prompt_config(base: &Config) -> GlobalConfig {
    Arc::new(RwLock::new(base.fork_session_scope()))
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

    let mut input = input::from_str(prompt_config, prompt_text, None);
    input::set_agent(&mut input, prompt_config, agent.into_config());
    Ok(input)
}

pub fn build_loop_ctx(
    prompt_config: GlobalConfig,
    call_fn: Option<AgentCallFn>,
) -> AgentLoopContext {
    AgentLoopContext {
        config: prompt_config,
        abort_signal: create_abort_signal(),
        async_manager: Arc::new(tokio::sync::Mutex::new(AsyncHookManager::default())),
        persistent_manager: Arc::new(tokio::sync::Mutex::new(PersistentHookManager::default())),
        call_fn,
        on_tool_round: None,
        on_text_response: None,
        initial_with_embeddings: true,
        initial_resume_count: 0,
        max_resume: None,
        pending_async_context: None,
    }
}

pub async fn ag_ui_run_with_call_fn(
    base_config: &Config,
    agent: &str,
    session: &str,
    req_body: &[u8],
    call_fn: Option<AgentCallFn>,
) -> Result<AppResponse, AgUiError> {
    let run_input = parse_run_input(req_body)?;
    resolve_agent(base_config, agent)?;

    let prompt_config = fork_prompt_config(base_config);
    {
        let mut config = prompt_config.write();
        config
            .use_agent_by_name(agent)
            .map_err(|e| AgUiError::Internal(format!("failed to set agent: {e}")))?;
        config
            .use_session(Some(session))
            .map_err(|e| AgUiError::Internal(format!("failed to use session: {e}")))?;
    }
    let persisted_history = prompt_config
        .read()
        .session
        .as_ref()
        .map(|session| {
            session
                .messages
                .iter()
                .filter(|msg| msg.role.is_user() || msg.role.is_assistant())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let new_messages = reconcile_new_messages(&persisted_history, &run_input.messages);
    if new_messages.is_empty() {
        return Err(AgUiError::BadRequest(
            "no new user message; session already up to date".to_string(),
        ));
    }
    let assistant_replays = new_messages
        .iter()
        .take_while(|message| message.role == Role::Assistant)
        .count();
    let new_user_messages = new_messages[assistant_replays..]
        .iter()
        .filter(|message| message.role == Role::User)
        .count();
    if new_user_messages > 1 {
        return Err(AgUiError::BadRequest(
            "multiple new messages are not supported in Phase 1; send exactly one new user message per run"
                .to_string(),
        ));
    }
    let new_message = new_messages[assistant_replays..]
        .iter()
        .find(|message| message.role == Role::User)
        .ok_or_else(|| {
            AgUiError::BadRequest("the new message must be a user message".to_string())
        })?;
    let prompt_text = new_message.content.clone();

    let message_id = MessageId::random();
    let mut input = build_local_input(&prompt_config, agent, session, &prompt_text)?;
    input.set_preferred_assistant_message_id(message_id.to_string());
    let thread_id = derive_thread_id(session);
    let run_id = run_id_from_input(&run_input);

    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<Event>();
    let sink = Arc::new(AgUiSink::new(evt_tx.clone(), message_id.clone()));

    evt_tx
        .send(Event::RunStarted(RunStartedEvent {
            base: BaseEvent {
                timestamp: None,
                raw_event: None,
            },
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
        }))
        .map_err(|err| AgUiError::Internal(format!("failed to queue RUN_STARTED event: {err}")))?;
    evt_tx
        .send(Event::TextMessageStart(TextMessageStartEvent {
            base: BaseEvent {
                timestamp: None,
                raw_event: None,
            },
            message_id: message_id.clone(),
            role: Role::Assistant,
        }))
        .map_err(|err| {
            AgUiError::Internal(format!("failed to queue TEXT_MESSAGE_START event: {err}"))
        })?;

    let loop_ctx = build_loop_ctx(prompt_config, call_fn);
    let done_tx = evt_tx.clone();
    tokio::spawn(async move {
        let loop_result = with_agent_event_sink(sink, async {
            Box::pin(run_agent_loop(&loop_ctx, input)).await
        })
        .await;

        match loop_result {
            Ok(()) => {
                let _ = done_tx.send(Event::TextMessageEnd(TextMessageEndEvent {
                    base: BaseEvent {
                        timestamp: None,
                        raw_event: None,
                    },
                    message_id,
                }));
                let _ = done_tx.send(Event::RunFinished(RunFinishedEvent {
                    base: BaseEvent {
                        timestamp: None,
                        raw_event: None,
                    },
                    thread_id,
                    run_id,
                    result: None,
                }));
            }
            Err(err) => {
                let _ = done_tx.send(Event::RunError(RunErrorEvent {
                    base: BaseEvent {
                        timestamp: None,
                        raw_event: None,
                    },
                    message: err.to_string(),
                    code: None,
                }));
            }
        }
    });
    drop(evt_tx);

    let stream = UnboundedReceiverStream::new(evt_rx).map(|event| {
        let frame = frame_event(&event).expect("AG-UI event framing should serialize");
        Ok::<_, Infallible>(Frame::data(Bytes::from(frame)))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .body(BodyExt::boxed(StreamBody::new(stream)))
        .map_err(|err| AgUiError::Internal(format!("failed to build AG-UI response: {err}")))
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
    content.to_text()
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
    use anyhow::anyhow;
    use harnx_core::{
        api_types::CompletionTokenUsage,
        event::{AgentEventSink, ModelEvent},
    };
    use harnx_runtime::{
        client::ToolCall,
        config::{Config, WorkingMode},
    };
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::{
        fs,
        path::PathBuf,
        sync::{LazyLock, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn ag_ui_sink_emits_text_message_content_for_chunk() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let message_id =
            MessageId::from(uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap());
        let sink = super::AgUiSink::new(tx, message_id.clone());

        sink.emit(
            AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text("hello".to_string())],
            }),
            None,
        );

        let event = rx.try_recv().expect("should receive event");
        match event {
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
            _ => panic!("expected TextMessageContent event, got: {:?}", event),
        }

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ag_ui_sink_skips_empty_chunk() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
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
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
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
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
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
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::new(tx, message_id.clone());

        let usage = CompletionTokenUsage::new(Some(10), Some(5), Some(0));
        sink.emit(
            AgentEvent::Model(ModelEvent::Final {
                output: "final text".to_string(),
                usage,
            }),
            None,
        );

        let event = rx.try_recv().expect("should receive event");
        match event {
            Event::TextMessageContent(TextMessageContentEvent {
                base,
                message_id: mid,
                delta,
            }) => {
                assert_eq!(mid, message_id);
                assert_eq!(delta, "final text");
                assert_eq!(base.timestamp, None);
            }
            _ => panic!("expected TextMessageContent event, got: {:?}", event),
        }
    }

    #[test]
    fn ag_ui_sink_skips_final_when_empty() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
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
    fn ag_ui_sink_drops_thought_chunk() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let message_id = MessageId::from(uuid::Uuid::new_v4());
        let sink = super::AgUiSink::new(tx, message_id);

        sink.emit(
            AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text("thinking...".to_string())],
            }),
            None,
        );

        assert!(
            rx.try_recv().is_err(),
            "ThoughtChunk should be dropped in Phase-1"
        );
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
    fn parse_run_input_rejects_empty_messages() {
        let body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "state": {},
            "messages": [],
            "tools": [],
            "context": [],
            "forwardedProps": {}
        });

        let err = parse_run_input(&serde_json::to_vec(&body).unwrap())
            .expect_err("empty messages should fail");
        assert_eq!(
            err,
            AgUiError::BadRequest("AG-UI request must include at least one message".to_string())
        );
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
    fn reconcile_new_messages_returns_only_trailing_new_suffix() {
        let persisted = vec![
            HistoryMsg::new(MessageRole::User, MessageContent::Text("hello".to_string())),
            HistoryMsg::new(
                MessageRole::Assistant,
                MessageContent::Text("hi".to_string()),
            ),
        ];
        let client_msgs = vec![
            user_msg("hello"),
            assistant_msg("hi"),
            user_msg("what next?"),
        ];

        let new_messages = reconcile_new_messages(&persisted, &client_msgs);
        assert_eq!(
            new_messages,
            vec![NewMsg {
                role: Role::User,
                content: "what next?".to_string(),
            }]
        );
    }

    #[test]
    fn reconcile_new_messages_returns_verified_suffix_after_divergence() {
        let persisted = vec![
            HistoryMsg::new(MessageRole::User, MessageContent::Text("old".to_string())),
            HistoryMsg::new(
                MessageRole::Assistant,
                MessageContent::Text("assistant-old".to_string()),
            ),
        ];
        let client_msgs = vec![assistant_msg("fresh assistant"), user_msg("last user wins")];

        let new_messages = reconcile_new_messages(&persisted, &client_msgs);
        assert_eq!(
            new_messages,
            vec![
                NewMsg {
                    role: Role::Assistant,
                    content: "fresh assistant".to_string(),
                },
                NewMsg {
                    role: Role::User,
                    content: "last user wins".to_string(),
                },
            ]
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
        let prompt_config = fork_prompt_config(&base_config);
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

        let response =
            ag_ui_run_with_call_fn(&config, "hephaestus", session_id, &req_body, Some(call_fn))
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

        let collected =
            futures_util::StreamExt::collect::<Vec<_>>(response.into_body().into_data_stream())
                .await
                .into_iter()
                .map(|chunk| {
                    String::from_utf8(chunk.expect("stream chunk").to_vec()).expect("utf8")
                })
                .collect::<Vec<_>>();

        let parsed_events = collected
            .iter()
            .map(|frame| {
                assert!(frame.starts_with("data: "));
                assert!(!frame.contains("event:"));
                let json = frame
                    .strip_prefix("data: ")
                    .and_then(|v| v.strip_suffix("\n\n"))
                    .expect("SSE data frame");
                serde_json::from_str::<serde_json::Value>(json).expect("valid event json")
            })
            .collect::<Vec<_>>();

        let event_types = parsed_events
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            vec![
                "RUN_STARTED",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "RUN_FINISHED",
            ]
        );

        let thread_id = derive_thread_id(session_id).to_string();
        let run_id = run_id_uuid.to_string();
        assert_eq!(
            parsed_events[0]["threadId"].as_str(),
            Some(thread_id.as_str())
        );
        assert_eq!(parsed_events[0]["runId"].as_str(), Some(run_id.as_str()));
        assert_eq!(parsed_events[2]["delta"].as_str(), Some("chunk-text"));
        assert_eq!(
            parsed_events[4]["threadId"].as_str(),
            Some(thread_id.as_str())
        );
        assert_eq!(parsed_events[4]["runId"].as_str(), Some(run_id.as_str()));

        let start_message_id = parsed_events[1]["messageId"].as_str().unwrap().to_string();
        assert_eq!(
            parsed_events[2]["messageId"].as_str(),
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

        let response = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&body).unwrap(),
            Some(call_fn),
        )
        .await
        .expect("ag ui response");
        let parsed_events = parse_sse_events(response).await;
        let event_types = parsed_events
            .iter()
            .map(|event| event["type"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            vec!["RUN_STARTED", "TEXT_MESSAGE_START", "RUN_ERROR"]
        );
        assert_eq!(
            parsed_events[2]["message"].as_str(),
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

        let response = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&body).unwrap(),
            Some(call_fn),
        )
        .await
        .expect("persisted run response");
        assert_run_finished(parse_sse_events(response).await);

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

        let response = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&body).unwrap(),
            Some(call_fn),
        )
        .await
        .expect("run response");
        let events = parse_sse_events(response).await;
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
        let wire_message_id = wire_message_ids[0].clone();

        let persisted_messages = load_session_messages(&config, "plain", session_id);
        let persisted_assistant = persisted_messages
            .iter()
            .find(|msg| msg.role.is_assistant())
            .expect("persisted assistant message");
        assert_eq!(
            persisted_assistant.id.as_deref(),
            Some(wire_message_id.as_str())
        );
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
    async fn ag_ui_run_exact_resend_returns_bad_request_without_duplication() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "exact-resend-session";

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
        let first_response = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&first_body).unwrap(),
            Some(first_call_fn),
        )
        .await
        .expect("first run");
        assert_run_finished(parse_sse_events(first_response).await);
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

        let resend_body = json!({
            "threadId": Uuid::new_v4(),
            "runId": Uuid::new_v4(),
            "messages": [
                {
                    "id": Uuid::new_v4(),
                    "role": "user",
                    "content": "user1"
                },
                {
                    "id": persisted_assistant_id,
                    "role": "assistant",
                    "content": "assistant1"
                }
            ]
        });

        let err = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&resend_body).unwrap(),
            None,
        )
        .await
        .expect_err("exact resend should fail");
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(
            load_session_texts(&config, "plain", session_id),
            first_messages
        );
    }

    #[tokio::test]
    async fn ag_ui_run_rejects_multiple_new_messages() {
        let sandbox = TestConfigSandbox::new();
        sandbox.write_agent("plain", "You are plain.");
        let config = sandbox.config();
        let session_id = "multi-tail-session";

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
        let first_response = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&first_body).unwrap(),
            Some(first_call_fn),
        )
        .await
        .expect("first run");
        assert_run_finished(parse_sse_events(first_response).await);
        assert_eq!(
            load_session_texts(&config, "plain", session_id),
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
                    "id": Uuid::new_v4(),
                    "role": "user",
                    "content": "user2"
                },
                {
                    "id": Uuid::new_v4(),
                    "role": "user",
                    "content": "user3"
                }
            ]
        });
        let second_call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
            Box::pin(async {
                let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
                Ok(("assistant2".into(), None, vec![], usage))
            })
        });
        let err = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&second_body).unwrap(),
            Some(second_call_fn),
        )
        .await
        .expect_err("multiple new messages should be rejected");
        assert_bad_request_contains(
            &err,
            "multiple new messages are not supported in Phase 1; send exactly one new user message per run",
        );
        assert_eq!(
            load_session_texts(&config, "plain", session_id),
            vec!["user1".to_string(), "assistant1".to_string()]
        );
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
        let first_response = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&first_body).unwrap(),
            Some(first_call_fn),
        )
        .await
        .expect("first run");
        assert_run_finished(parse_sse_events(first_response).await);
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
        let second_response = ag_ui_run_with_call_fn(
            &config,
            "plain",
            session_id,
            &serde_json::to_vec(&second_body).unwrap(),
            Some(second_call_fn),
        )
        .await
        .expect("second run");
        assert_run_finished(parse_sse_events(second_response).await);
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
            .map(|frame| {
                let payload = frame
                    .strip_prefix("data: ")
                    .expect("sse frame should start with data prefix");
                serde_json::from_str(payload).expect("frame should be valid json")
            })
            .collect()
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
        let prompt_config = fork_prompt_config(config);
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
        let prompt_config = fork_prompt_config(config);
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

    static TEST_CONFIG_DIR_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct TestConfigSandbox {
        _lock: std::sync::MutexGuard<'static, ()>,
        root: PathBuf,
        data_dir: PathBuf,
        state_dir: PathBuf,
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl TestConfigSandbox {
        fn new() -> Self {
            let lock = TEST_CONFIG_DIR_LOCK.lock().expect("test config dir lock");
            let root = unique_test_config_dir();
            let data_dir = root.join("data");
            let state_dir = root.join("state");

            fs::create_dir_all(root.join("clients")).expect("create clients dir");
            fs::create_dir_all(root.join("agents")).expect("create agents dir");
            fs::create_dir_all(&data_dir).expect("create data dir");
            fs::create_dir_all(&state_dir).expect("create state dir");
            fs::write(
                root.join("config.yaml"),
                "model: openai:gpt-4o\nsave_session: true\n",
            )
            .expect("write config");
            fs::write(
                root.join("clients/openai.yaml"),
                concat!(
                    "type: openai\n",
                    "api_key: sk-test\n",
                    "models:\n",
                    "  - name: gpt-4o\n",
                    "    type: chat\n",
                    "    max_input_tokens: 4096\n"
                ),
            )
            .expect("write openai client");

            let vars = vec![
                ("HARNX_CONFIG_DIR", std::env::var_os("HARNX_CONFIG_DIR")),
                ("HARNX_DATA_DIR", std::env::var_os("HARNX_DATA_DIR")),
                ("HARNX_STATE_DIR", std::env::var_os("HARNX_STATE_DIR")),
            ];
            unsafe {
                std::env::set_var("HARNX_CONFIG_DIR", &root);
                std::env::set_var("HARNX_DATA_DIR", &data_dir);
                std::env::set_var("HARNX_STATE_DIR", &state_dir);
                std::env::remove_var("HARNX_CONFIG_FILE");
            }

            Self {
                _lock: lock,
                root,
                data_dir,
                state_dir,
                vars,
            }
        }

        fn config(&self) -> Config {
            let prev = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(&self.root).expect("switch cwd");
            let result = futures::executor::block_on(Config::init(WorkingMode::Cmd, false, vec![]));
            std::env::set_current_dir(prev).expect("restore cwd");
            result.expect("load config")
        }

        fn write_agent(&self, name: &str, prompt: &str) {
            let body = format!("---\nmodel: openai:gpt-4o\n---\n{prompt}\n");
            fs::write(self.root.join("agents").join(format!("{name}.md")), body)
                .expect("write agent");
        }
    }

    impl Drop for TestConfigSandbox {
        fn drop(&mut self) {
            for (key, previous) in &self.vars {
                match previous {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
            let _ = fs::remove_dir_all(&self.data_dir);
            let _ = fs::remove_dir_all(&self.state_dir);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn unique_test_config_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "harnx-serve-ag-ui-test-{}-{timestamp}",
            std::process::id()
        ))
    }
}
