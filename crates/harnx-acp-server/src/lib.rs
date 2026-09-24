//! harnx-acp-server — ACP server front-end over stdio with NATS binding.
//!
//! Implements ACP v1 protocol with:
//! - `initialize` negotiating protocol version 1
//! - `session/new` creating NATS-backed sessions via local worker
//! - `session/prompt` running turn with in-order streaming
//! - `session/cancel` notification to interrupt running turns
//!
//! Architecture follows harnx-serve pattern:
//! - Two-plane split: control plane (ACP requests) and event plane (session/update notifications)
//! - Off-loop prompt execution so cancel can be received mid-turn
//! - Single sequential drain loop for in-order streaming (PR #1038 fix)
//!
//! Phase 2 of #1346.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    ContentBlock as AcpContentBlock, ContentChunk, Implementation, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use anyhow::Context;
use harnx_core::abort::AbortSignal;
use harnx_core::agent_config::AgentConfig;
use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
use harnx_core::input::Input;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::local_orchestrator::{activation_route_for_cluster, LocalWorkerSupervisor};

use tracing::{debug, error, info, warn};

pub mod event_sink;
pub mod server_main;
pub mod session_context;

pub use event_sink::{AcpEventSink, AcpMessage, SignalHandle};
pub use server_main::run;
pub use session_context::{SessionContext, SESSION_IDLE_TTL};

/// Connection to ACP client for sending notifications.
pub type AcpConnection = acp::ConnectionTo<acp::Client>;

/// The ACP agent implementation for harnx.
///
/// Holds:
/// - Active sessions mapped by ACP session ID
/// - Local worker supervisor for NATS bootstrap
/// - Global abort signal for graceful shutdown
/// - Connection to ACP client for sending notifications
pub struct HarnxAgent {
    agent_name: String,
    sessions: Arc<tokio::sync::RwLock<HashMap<String, Arc<SessionContext>>>>,
    local_worker: Arc<tokio::sync::Mutex<Option<LocalWorkerSupervisor>>>,
    abort_signal: AbortSignal,
    /// Connection to ACP client for sending session/update notifications.
    /// Set by `set_connection()` when a request/notification arrives.
    connection: Arc<tokio::sync::RwLock<Option<AcpConnection>>>,
    /// Explicit backend used by embedded callers and integration tests.
    /// Production leaves this unset and uses the managed local worker.
    runtime_config: Option<GlobalConfig>,
    cluster: String,
    activation_route: Option<harnx_runtime::SessionActivationRoute>,
    session_initializer: Option<harnx_runtime::SessionInitializer>,
}

/// NATS backend settings for an embedded ACP agent.
pub struct NatsAgentConfig {
    /// Runtime configuration containing the selected NATS cluster.
    pub runtime_config: GlobalConfig,
    /// NATS cluster name used for session creation.
    pub cluster: String,
    /// Route used to activate newly admitted prompts.
    pub activation_route: harnx_runtime::SessionActivationRoute,
    /// Initial agent definition and session overrides.
    pub session_initializer: harnx_runtime::SessionInitializer,
}

impl HarnxAgent {
    /// Create a new agent instance with the given display name.
    pub fn new(agent_name: String) -> Self {
        Self {
            agent_name,
            sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            local_worker: Arc::new(tokio::sync::Mutex::new(None)),
            abort_signal: harnx_core::abort::create_abort_signal(),
            connection: Arc::new(tokio::sync::RwLock::new(None)),
            runtime_config: None,
            cluster: harnx_runtime::config::LOCAL_CLUSTER_KEY.to_string(),
            activation_route: None,
            session_initializer: None,
        }
    }

    /// Create an agent bound to an already-running NATS cluster.
    ///
    /// This avoids starting a second broker or worker when the ACP server is
    /// embedded in another process. The supplied config must resolve `cluster`.
    pub fn with_nats_config(agent_name: String, config: NatsAgentConfig) -> Self {
        Self {
            agent_name,
            sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            local_worker: Arc::new(tokio::sync::Mutex::new(None)),
            abort_signal: harnx_core::abort::create_abort_signal(),
            connection: Arc::new(tokio::sync::RwLock::new(None)),
            runtime_config: Some(config.runtime_config),
            cluster: config.cluster,
            activation_route: Some(config.activation_route),
            session_initializer: Some(config.session_initializer),
        }
    }

    /// Store the ACP connection for sending notifications.
    pub async fn set_connection(&self, cx: AcpConnection) {
        *self.connection.write().await = Some(cx);
    }

    /// Get the current ACP connection.
    pub async fn get_connection(&self) -> Option<AcpConnection> {
        self.connection.read().await.clone()
    }

    /// Handle `initialize` request — negotiate protocol v1, advertise minimal caps.
    pub async fn initialize(&self, _request: InitializeRequest) -> acp::Result<InitializeResponse> {
        // We only support v1 (default from SDK "2.2.0", NOT unstable_protocol_v2).
        // Always advertise v1 instead of echoing an unsupported client version.
        Ok(
            InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                .agent_capabilities(AgentCapabilities::new())
                .agent_info(
                    Implementation::new("harnx".to_string(), env!("CARGO_PKG_VERSION").to_string())
                        .title(self.agent_name.clone()),
                ),
        )
    }

    /// Handle `authenticate` request — no-op for now.
    pub async fn authenticate(
        &self,
        _request: AuthenticateRequest,
    ) -> acp::Result<AuthenticateResponse> {
        Ok(AuthenticateResponse::default())
    }

    /// Handle `session/new` — create NATS-backed session via local worker bootstrap.
    ///
    /// This boots the local worker supervisor if not already running,
    /// then creates a NatsSession bound to the local cluster.
    pub async fn new_session(
        &self,
        _request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        let (activation_route, global_config) =
            if let (Some(route), Some(config)) = (&self.activation_route, &self.runtime_config) {
                (route.clone(), Arc::clone(config))
            } else {
                let route = activation_route_for_cluster(
                    &self.cluster,
                    &self.local_worker,
                    self.abort_signal.clone(),
                )
                .await
                .context("failed to bootstrap local NATS worker")
                .map_err(acp_error)?;
                let config_path = harnx_runtime::config::Config::config_file();
                let mut config = harnx_runtime::config::Config::load_from_file(&config_path)
                    .context("failed to load config")
                    .map_err(acp_error)?;
                config.apply_frontend_nats_routing();
                (route, Arc::new(parking_lot::RwLock::new(config)))
            };

        let initializer = self.session_initializer.clone().unwrap_or_else(|| {
            harnx_runtime::SessionInitializer::named(self.agent_name.clone(), Default::default())
        });
        let session_config = harnx_runtime::NatsSessionConfig {
            cluster: self.cluster.clone(),
            initializer,
            session_id: None,
            activation_route,
        };

        let nats_session = harnx_runtime::NatsSession::from_global_config(
            session_config,
            &global_config,
            self.abort_signal.clone(),
        )
        .await
        .context("failed to create NATS session")
        .map_err(acp_error)?;

        let session_id = nats_session.session_id().to_string();

        // Wrap in server-side context with idle tracking
        let session_ctx = Arc::new(SessionContext::new(nats_session));

        // Touch on creation to mark as active
        session_ctx.touch();

        // Store session
        self.sessions
            .write()
            .await
            .insert(session_id.clone(), session_ctx);

        debug!(session_id = %session_id, "created new ACP session");
        Ok(NewSessionResponse::new(SessionId::new(session_id)))
    }

    /// Handle `session/prompt` — run turn with in-order streaming via session/update.
    ///
    /// Implementation follows harnx-serve pattern:
    /// 1. Touch session for idle tracking (PR #1003 fix)
    /// 2. Parse user content from ACP request
    /// 3. Create event sink with drain channel
    /// 4. Spawn drain task for in-order streaming (PR #1038 fix)
    /// 5. Run prompt off-loop via admit_input + follow_admitted_prompt
    /// 6. Await drain task completion before responding
    ///
    /// CRITICAL: NO per-chunk tokio::spawn. All chunks flow through single drain loop.
    pub async fn prompt(&self, request: PromptRequest) -> acp::Result<PromptResponse> {
        let session_id = request.session_id.0.to_string();
        let session_ctx = self.get_session(&session_id).await?;
        session_ctx.touch();

        let (turn_guard, cancel_rx) = session_ctx.begin_turn().ok_or_else(|| {
            acp_error(anyhow::anyhow!(
                "session already has an in-flight turn: {session_id}"
            ))
        })?;
        let input = Input::new(
            parse_prompt_content(&request),
            (String::new(), vec![]),
            AgentConfig::default(),
        );
        let (sink, drain_rx) = AcpEventSink::new(session_id.clone());
        let sink = Arc::new(sink);
        let drain_handle = tokio::spawn(drain_updates(self.get_connection().await, drain_rx));

        let result = run_prompt_turn(&session_ctx, input, Arc::clone(&sink), cancel_rx).await;
        sink.signal_complete();
        finish_update_drain(drain_handle).await;
        turn_guard.finish();

        handle_turn_result(&session_id, result)
    }

    async fn get_session(&self, session_id: &str) -> acp::Result<Arc<SessionContext>> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| acp_error(anyhow::anyhow!("session not found: {session_id}")))
    }

    /// Return the server-owned activity timestamp for a session.
    pub async fn session_last_touched(&self, session_id: &str) -> Option<Duration> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|session| session.last_touched())
    }

    /// Handle `session/cancel` notification — interrupt running turn.
    ///
    /// Calls cancel_pending_turn on the NATS session and marks cancellation
    /// in the ACP protocol response.
    pub async fn cancel(&self, notification: CancelNotification) -> anyhow::Result<()> {
        let session_id = notification.session_id.0.as_ref();

        let session_ctx = {
            let sessions = self.sessions.read().await;
            sessions
                .get(session_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("session not found: {}", session_id))?
        };

        session_ctx.touch();
        let local_cancelled = session_ctx.cancel_local_turn();
        let nats_cancelled = session_ctx
            .nats_session()
            .cancel_pending_turn()
            .await
            .context("failed to cancel pending turn")?;

        info!(
            session_id = %session_id,
            local_cancelled,
            nats_cancelled,
            "session cancel processed"
        );

        Ok(())
    }
}

async fn run_prompt_turn(
    session: &SessionContext,
    input: Input,
    sink: Arc<AcpEventSink>,
    cancel_rx: tokio::sync::mpsc::Receiver<()>,
) -> acp::Result<harnx_runtime::NatsTurnResult> {
    let nats_session = session.nats_session();
    let appended = nats_session
        .admit_input(&input, None)
        .await
        .context("failed to admit prompt input")
        .map_err(acp_error)?;
    nats_session
        .follow_admitted_prompt(
            appended,
            sink,
            Some(cancel_rx),
            None,
            harnx_runtime::RunTurnOptions::default(),
        )
        .await
        .context("prompt turn failed")
        .map_err(acp_error)
}

fn forward_update(connection: Option<&AcpConnection>, session_id: &str, event: AcpEvent) {
    let Some(connection) = connection else {
        return;
    };
    let Some(notification) = event_to_session_notification(session_id, event) else {
        return;
    };
    if let Err(error) = connection.send_notification(notification) {
        warn!(%error, "failed to send ACP session update");
    }
}

async fn drain_updates(
    connection: Option<AcpConnection>,
    mut updates: tokio::sync::mpsc::UnboundedReceiver<AcpMessage>,
) {
    while let Some(message) = updates.recv().await {
        match message {
            AcpMessage::Update { session_id, event } => {
                forward_update(connection.as_ref(), &session_id, event);
            }
            AcpMessage::TurnComplete => break,
        }
    }
}

async fn finish_update_drain(drain_handle: tokio::task::JoinHandle<()>) {
    if let Err(error) = drain_handle.await {
        error!(%error, "ACP update drain task panicked");
    }
}

fn handle_turn_result(
    session_id: &str,
    result: acp::Result<harnx_runtime::NatsTurnResult>,
) -> acp::Result<PromptResponse> {
    let turn_result = result?;
    if turn_result.was_cancelled {
        debug!(%session_id, "prompt turn cancelled");
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }
    if let Some(error) = turn_result.error {
        return Err(acp_error(anyhow::anyhow!(error)));
    }
    debug!(%session_id, "prompt turn completed");
    Ok(PromptResponse::new(StopReason::EndTurn))
}

/// Event types that can be forwarded to ACP client.
#[derive(Debug)]
pub enum AcpEvent {
    /// Text chunk for AgentMessageChunk.
    Text(String),
    /// Error chunk with harnx:error flag (PR #1128 fix).
    Error(String),
}

/// Convert AgentEvent to AcpEvent for streaming.
pub(crate) fn agent_event_to_acp_event(event: AgentEvent) -> Option<AcpEvent> {
    match event {
        AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
            let text: String = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.clone()),
                    _ => None,
                })
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(AcpEvent::Text(text))
            }
        }
        AgentEvent::Model(ModelEvent::Error(msg)) => {
            // PR #1128 fix: Model errors flagged with harnx:error meta
            Some(AcpEvent::Error(msg))
        }
        _ => None,
    }
}

/// Convert an AcpEvent to a SessionNotification.
fn event_to_session_notification(session_id: &str, event: AcpEvent) -> Option<SessionNotification> {
    match event {
        AcpEvent::Text(text) => {
            let chunk = ContentChunk::new(AcpContentBlock::Text(TextContent::new(text)));
            let notification = SessionNotification::new(
                SessionId::new(session_id.to_string()),
                SessionUpdate::AgentMessageChunk(chunk),
            );
            Some(notification)
        }
        AcpEvent::Error(msg) => {
            // PR #1128 fix: flag with harnx:error meta
            let mut meta = serde_json::Map::new();
            meta.insert("harnx:error".to_string(), serde_json::Value::Bool(true));
            let chunk = ContentChunk::new(AcpContentBlock::Text(TextContent::new(msg))).meta(meta);
            let notification = SessionNotification::new(
                SessionId::new(session_id.to_string()),
                SessionUpdate::AgentMessageChunk(chunk),
            );
            Some(notification)
        }
    }
}

/// Parse user message content from PromptRequest.
///
/// ACP PromptRequest contains a `prompt` field with a list of content blocks.
/// For Phase 2, we extract just the text content.
fn parse_prompt_content(request: &PromptRequest) -> String {
    request
        .prompt
        .iter()
        .filter_map(|block| {
            // In ACP v1, ContentBlock has variants Text, Image, Audio, ResourceLink, Resource
            // Extract text from Text variant only
            match block {
                agent_client_protocol::schema::v1::ContentBlock::Text(text_content) => {
                    Some(text_content.text.clone())
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Convert an anyhow error to an ACP protocol error.
fn acp_error(e: anyhow::Error) -> acp::Error {
    // Use JSON-RPC internal error code
    acp::Error::new(-32603, format!("{:#}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> HarnxAgent {
        HarnxAgent::new("test-agent".to_string())
    }

    #[tokio::test]
    async fn initialize_returns_protocol_version_1_for_unsupported_requests() {
        let agent = test_agent();

        for requested_version in [2_u16, 99] {
            let request = InitializeRequest::new(
                agent_client_protocol::schema::ProtocolVersion::from(requested_version),
            );
            let response = agent.initialize(request).await.unwrap();

            assert_eq!(
                response.protocol_version,
                agent_client_protocol::schema::ProtocolVersion::V1
            );
        }
    }

    #[tokio::test]
    async fn initialize_advertises_minimal_capabilities() {
        let agent = test_agent();
        let request = InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1);

        let response = agent.initialize(request).await.unwrap();

        assert!(response.agent_info.is_some());
        let agent_info = response.agent_info.unwrap();
        assert_eq!(agent_info.name, "harnx");
        assert!(agent_info.title.is_some());
        assert_eq!(agent_info.title.unwrap(), "test-agent");
    }

    #[tokio::test]
    async fn authenticate_responds_ok() {
        let agent = test_agent();
        let method_id = agent_client_protocol::schema::v1::AuthMethodId::new("agent".to_string());
        let request = AuthenticateRequest::new(method_id);

        let response = agent.authenticate(request).await.unwrap();
        let _ = response;
    }

    #[test]
    fn parse_prompt_content_extracts_text() {
        use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
        let text_content = TextContent::new("hello world".to_string());
        let request = PromptRequest::new(
            SessionId::new("test".to_string()),
            vec![ContentBlock::Text(text_content)],
        );

        let content = parse_prompt_content(&request);
        assert_eq!(content, "hello world");
    }
}
