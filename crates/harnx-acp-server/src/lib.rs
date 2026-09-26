//! harnx-acp-server — ACP server front-end over stdio with NATS binding.
//!
//! Implements ACP v1 protocol with:
//! - `initialize` negotiating protocol version 1
//! - `session/new` creating NATS-backed sessions via local worker
//! - `session/load` replaying durable transcript and establishing session context
//! - `session/resume` establishing context without replaying history
//! - `session/list` discovering pinned-agent sessions on the configured cluster
//! - `session/close` removing context while preserving durable history
//! - `session/prompt` running turn with in-order streaming
//! - `session/request_permission` bridging gated tools to ACP clients
//! - committed handoffs producing an actionable fallback and deactivating source
//! - `session/cancel` notification to interrupt running turns
//!
//! Architecture follows harnx-serve pattern:
//! - Two-plane split: control plane (ACP requests) and event plane (session/update notifications)
//! - Off-loop prompt execution so cancel can be received mid-turn
//! - Single sequential drain loop for in-order streaming (PR #1038 fix)
//!
//! Capability-gated ACP bridge for #1346.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification,
    CloseSessionRequest, CloseSessionResponse, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionId, SessionInfo, SessionListCapabilities, SessionNotification,
    SessionResumeCapabilities, SessionUpdate, StopReason,
};
use anyhow::Context;
use harnx_core::abort::AbortSignal;
use harnx_core::agent_config::AgentConfig;
use harnx_core::input::Input;
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::local_orchestrator::{activation_route_for_cluster, LocalWorkerSupervisor};

use tracing::{debug, error, info, warn};

pub mod event_map;
pub mod event_sink;
pub mod handoff;
pub mod permission;
pub mod server_main;
pub mod session_context;

pub use event_sink::{AcpEventSink, AcpMessage, SignalHandle};
pub use handoff::HandoffTarget;
pub use server_main::run;
use session_context::BeginTurnError;
pub use session_context::{SessionContext, SESSION_IDLE_TTL};

/// Connection to ACP client for sending notifications and permission requests.
pub type AcpConnection = acp::ConnectionTo<acp::Client>;

/// Metadata key marking an ACP content chunk as a harnx model error.
pub const HARNX_ERROR_META: &str = "harnx:error";
/// Metadata key carrying harnx's pre-rendered tool-call markdown.
pub const HARNX_MARKDOWN_META: &str = "harnx:markdown";
/// Metadata key carrying the latest per-call token usage snapshot.
pub const HARNX_USAGE_META: &str = "harnx:usage";
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
                .agent_capabilities(
                    AgentCapabilities::new()
                        .load_session(true)
                        .session_capabilities(
                            SessionCapabilities::new()
                                .list(SessionListCapabilities::new())
                                .resume(SessionResumeCapabilities::new())
                                .close(SessionCloseCapabilities::new()),
                        ),
                )
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

    async fn backend_config(
        &self,
    ) -> acp::Result<(harnx_runtime::SessionActivationRoute, GlobalConfig)> {
        if let (Some(route), Some(config)) = (&self.activation_route, &self.runtime_config) {
            return Ok((route.clone(), Arc::clone(config)));
        }
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
        Ok((route, Arc::new(parking_lot::RwLock::new(config))))
    }

    /// Handle `session/new` — create NATS-backed session via local worker bootstrap.
    ///
    /// This boots the local worker supervisor if not already running,
    /// then creates a NatsSession bound to the local cluster.
    pub async fn new_session(&self, request: NewSessionRequest) -> acp::Result<NewSessionResponse> {
        // IDEs inject their own MCP servers here. Harnx uses its configured tool
        // servers, so accepting and ignoring these entries is intentional.
        debug!(
            mcp_server_count = request.mcp_servers.len(),
            "creating ACP session"
        );
        let (activation_route, global_config) = self.backend_config().await?;

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

    /// Handle `session/load` by replaying durable transcript and establishing session context.
    ///
    /// After replaying the historical transcript as an ordered snapshot, establishes a
    /// `SessionContext` so subsequent `session/prompt` and `session/cancel` calls succeed.
    /// If replay or validation fails, no session context is inserted.
    pub async fn load_session(
        &self,
        request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        let session_id = request.session_id.0.to_string();
        if session_id.trim().is_empty() {
            return Err(acp_error(anyhow::anyhow!("session ID must not be empty")));
        }
        debug!(
            %session_id,
            mcp_server_count = request.mcp_servers.len(),
            additional_directory_count = request.additional_directories.len(),
            "loading ACP session from durable transcript"
        );
        let connection = self.get_connection().await.ok_or_else(|| {
            acp_error(anyhow::anyhow!(
                "ACP client connection unavailable for session replay"
            ))
        })?;
        let entries = self.load_scoped_entries(&session_id).await?;
        let (sink, drain_rx) = AcpEventSink::for_replay(session_id.clone(), self.cluster.clone());
        let sink = Arc::new(sink);
        let drain_handle = tokio::spawn(drain_updates(Some(connection), drain_rx));

        harnx_runtime::replay_entries_to_sink(&entries, sink.clone());
        sink.signal_complete();
        finish_update_drain(drain_handle).await;

        // Establish session context for subsequent prompt/cancel operations
        self.establish_session_context(&session_id, &entries)
            .await?;
        debug!(session_id = %session_id, "established session context after load");

        Ok(LoadSessionResponse::new())
    }

    /// Establish a `SessionContext` for an existing loaded session.
    ///
    /// This mirrors `new_session` but reuses an existing session ID from durable storage.
    /// Called after successful replay validation in `load_session`.
    ///
    /// If the replayed entries contain a `HandoffCommitted` record, the session is
    /// deactivated (rejects future prompts) by applying the handoff target to the context.
    async fn establish_session_context(
        &self,
        session_id: &str,
        entries: &[(u64, harnx_core::session::SessionLogEntry)],
    ) -> acp::Result<()> {
        let (activation_route, global_config) = self.backend_config().await?;

        let initializer = self.session_initializer.clone().unwrap_or_else(|| {
            harnx_runtime::SessionInitializer::named(self.agent_name.clone(), Default::default())
        });
        let session_config = harnx_runtime::NatsSessionConfig {
            cluster: self.cluster.clone(),
            initializer,
            session_id: Some(session_id.to_string()),
            activation_route,
        };

        let nats_session = harnx_runtime::NatsSession::from_global_config(
            session_config,
            &global_config,
            self.abort_signal.clone(),
        )
        .await
        .context("failed to create NATS session for loaded session")
        .map_err(acp_error)?;

        let session_ctx = Arc::new(SessionContext::new(nats_session));
        session_ctx.touch();

        // Rehydrate handoff state from durable transcript.
        // If the session was committed as handed off, mark it deactivated.
        if let Some(target) = find_handoff_target(entries, &self.cluster) {
            session_ctx.commit_handoff(target);
        }

        self.sessions
            .write()
            .await
            .insert(session_id.to_string(), session_ctx);

        Ok(())
    }

    async fn load_scoped_entries(
        &self,
        session_id: &str,
    ) -> acp::Result<Vec<(u64, harnx_core::session::SessionLogEntry)>> {
        let (_route, global_config) = self.backend_config().await?;
        let config = global_config.read().clone();
        let agent_ref = self.scoped_agent_ref()?;
        let (jetstream, metadata) =
            harnx_runtime::config::session_metadata_for_agent(&config, &agent_ref, session_id)
                .await
                .context("failed to resolve scoped session identity")
                .map_err(acp_error)?;
        let entries =
            harnx_runtime::nats_session_log::NatsSessionLog::new(jetstream, metadata.storage_key())
                .load_events_async()
                .await
                .context("failed to load durable session transcript")
                .map_err(acp_error)?;
        harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)
            .context("failed to reconstruct durable session transcript")
            .map_err(acp_error)
    }

    fn scoped_agent_ref(&self) -> acp::Result<String> {
        use harnx_core::agent_ref::AgentRef;

        match AgentRef::parse(&self.agent_name) {
            AgentRef::Local(agent) if self.cluster == harnx_runtime::config::LOCAL_CLUSTER_KEY => {
                Ok(agent.into_owned())
            }
            AgentRef::Local(agent) => Ok(format!("{agent}@{}", self.cluster)),
            AgentRef::Remote { agent, cluster } if cluster == self.cluster => {
                Ok(format!("{agent}@{cluster}"))
            }
            AgentRef::Remote { cluster, .. } => Err(acp_error(anyhow::anyhow!(
                "configured agent cluster '{cluster}' does not match ACP backend cluster '{}'",
                self.cluster
            ))),
        }
    }

    /// Handle `session/list` — return sessions for the pinned agent only.
    ///
    /// Queries the NATS metadata store for the current cluster, filters by agent ownership,
    /// and returns newest-first sessions matching the optional cwd filter.
    pub async fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        let (_route, global_config) = self.backend_config().await?;
        let config = global_config.read().clone();

        let jetstream = config
            .nats_jetstream(&self.cluster)
            .await
            .context("failed to connect to NATS")
            .map_err(acp_error)?;

        let replicas = config
            .nats_server(&self.cluster)
            .context("failed to resolve NATS server config")
            .map_err(acp_error)?
            .resolved_replicas();

        let metadata_store = harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(
            &jetstream, replicas,
        )
        .await
        .context("failed to access session metadata store")
        .map_err(acp_error)?;

        let listed_sessions = metadata_store
            .list()
            .await
            .context("failed to list sessions")
            .map_err(acp_error)?;

        // Filter to pinned agent only
        let sessions: Vec<SessionInfo> = listed_sessions
            .into_iter()
            .filter(|session| {
                harnx_runtime::nats_session_metadata::metadata_belongs_to_agent(
                    &session.metadata,
                    &self.agent_name,
                )
            })
            .filter_map(|session| {
                let cwd = extract_session_cwd(&session.metadata).unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
                });

                // Apply optional cwd filter (exact match)
                if let Some(ref filter_cwd) = request.cwd {
                    if &cwd != filter_cwd {
                        return None;
                    }
                }

                let title = session.metadata.title.value;
                let updated_at = session
                    .activity
                    .as_ref()
                    .map(|a| a.last_activity_at.to_rfc3339())
                    .or_else(|| Some(session.metadata.created_at.to_rfc3339()));

                Some(
                    SessionInfo::new(SessionId::new(session.metadata.session_id), cwd)
                        .title(title)
                        .updated_at(updated_at),
                )
            })
            .collect();

        // Sessions are already sorted newest-first by SessionMetadataStore::list()
        // The backend ordering is preserved

        Ok(ListSessionsResponse::new(sessions))
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
        let (turn_guard, cancel_rx) = match session_ctx.begin_turn() {
            Ok(turn) => turn,
            Err(BeginTurnError::Active) => {
                session_ctx.touch();
                return Err(acp_error(anyhow::anyhow!(
                    "session already has an in-flight turn: {session_id}"
                )));
            }
            Err(BeginTurnError::HandedOff(target)) => {
                return Err(acp_error(anyhow::anyhow!(target.prompt_rejection())));
            }
        };
        session_ctx.touch();
        let input = Input::new(
            parse_prompt_content(&request),
            (String::new(), vec![]),
            AgentConfig::default(),
        );
        let (sink, drain_rx) = AcpEventSink::for_session(
            session_id.clone(),
            self.cluster.clone(),
            Arc::clone(&session_ctx),
        );
        let sink = Arc::new(sink);
        let connection = self.get_connection().await;
        let drain_handle = tokio::spawn(drain_updates(connection.clone(), drain_rx));
        let turn = PromptTurn {
            input,
            sink: Arc::clone(&sink),
            cancel_rx,
            confirmation_handler: permission::tool_confirmation_handler(
                connection,
                session_id.clone(),
            ),
        };

        let result = run_prompt_turn(&session_ctx, turn).await;
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

    /// Return authoritative target after source commits a handoff.
    pub async fn session_handoff_target(&self, session_id: &str) -> Option<HandoffTarget> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .and_then(|session| session.handoff_target())
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

    /// Handle `session/resume` — establish context for existing session without replay.
    ///
    /// Validates ownership and existence, then establishes a `SessionContext` with
    /// handoff rehydration. Unlike `load_session`, this does NOT replay history
    /// updates to the client.
    pub async fn resume_session(
        &self,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        let session_id = request.session_id.0.to_string();
        if session_id.trim().is_empty() {
            return Err(acp_error(anyhow::anyhow!("session ID must not be empty")));
        }

        debug!(%session_id, "resuming ACP session");

        // Validate ownership and existence by loading entries
        let entries = self.load_scoped_entries(&session_id).await?;

        // Establish session context for subsequent prompt/cancel (reuses load helper)
        self.establish_session_context(&session_id, &entries)
            .await?;

        debug!(%session_id, "resumed ACP session without replay");

        Ok(ResumeSessionResponse::new())
    }

    /// Handle `session/close` — remove session context, preserving durable history.
    ///
    /// Cancels any active turn and removes the `SessionContext` from in-memory storage.
    /// Durable NATS transcript, metadata, and listing visibility remain intact.
    /// Idempotent: closing unknown or already-closed session succeeds.
    pub async fn close_session(
        &self,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        let session_id = request.session_id.0.to_string();

        debug!(%session_id, "closing ACP session");

        if let Some(session_ctx) = self.sessions.write().await.remove(&session_id) {
            // Cancel local turn guard if active
            session_ctx.cancel_local_turn();
            // Cancel pending NATS turn
            let _ = session_ctx.nats_session().cancel_pending_turn().await;
            debug!(%session_id, "ACP session closed");
        } else {
            debug!(%session_id, "ACP session already closed or unknown (idempotent)");
        }

        Ok(CloseSessionResponse::new())
    }
}

struct PromptTurn {
    input: Input,
    sink: Arc<AcpEventSink>,
    cancel_rx: tokio::sync::mpsc::Receiver<()>,
    confirmation_handler: Arc<harnx_runtime::nats_tool_confirmation::ToolConfirmationHandler>,
}

async fn run_prompt_turn(
    session: &SessionContext,
    turn: PromptTurn,
) -> acp::Result<harnx_runtime::NatsTurnResult> {
    let nats_session = session.nats_session();
    let route = nats_session
        .tool_confirmation_route(turn.confirmation_handler)
        .await
        .context("failed to create tool confirmation route")
        .map_err(acp_error)?;
    let result = async {
        let appended = nats_session
            .admit_input_with_tool_confirmation_route(&turn.input, &route)
            .await
            .context("failed to admit prompt input")
            .map_err(acp_error)?;
        nats_session
            .follow_admitted_prompt(
                appended,
                turn.sink,
                Some(turn.cancel_rx),
                Some(route.subject()),
                harnx_runtime::RunTurnOptions::default(),
            )
            .await
            .context("prompt turn failed")
            .map_err(acp_error)
    }
    .await;
    route.close().await;
    result
}

async fn drain_updates(
    connection: Option<AcpConnection>,
    mut updates: tokio::sync::mpsc::UnboundedReceiver<AcpMessage>,
) {
    while let Some(message) = updates.recv().await {
        match message {
            AcpMessage::Update { session_id, update } => {
                forward_update(connection.as_ref(), &session_id, *update);
            }
            AcpMessage::TurnComplete => break,
        }
    }
}

fn forward_update(connection: Option<&AcpConnection>, session_id: &str, update: SessionUpdate) {
    let Some(connection) = connection else {
        return;
    };
    let notification = SessionNotification::new(SessionId::new(session_id.to_string()), update);
    if let Err(error) = connection.send_notification(notification) {
        warn!(%error, "failed to send ACP session update");
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

/// Find the most recent `HandoffCommitted` entry in the transcript and construct a `HandoffTarget`.
fn find_handoff_target(
    entries: &[(u64, harnx_core::session::SessionLogEntry)],
    source_cluster: &str,
) -> Option<HandoffTarget> {
    // Iterate in reverse to find the most recent handoff (highest seq)
    for (_seq, entry) in entries.iter().rev() {
        if let harnx_core::session::SessionLogEntry::HandoffCommitted {
            target_agent,
            target_session_id,
            handoff_tool_call_id: _,
        } = entry
        {
            if let Some(target) =
                HandoffTarget::from_committed(target_agent, target_session_id, source_cluster)
            {
                return Some(target);
            }
        }
    }
    None
}

/// Extract the working directory from session metadata.
///
/// Uses execution context observations (from tool calls) if available,
/// falling back to the process working directory if not recorded.
fn extract_session_cwd(
    metadata: &harnx_runtime::nats_session_metadata::SessionMetadata,
) -> Option<std::path::PathBuf> {
    harnx_runtime::nats_session_metadata::execution_contexts(metadata)
        .ok()
        .and_then(|contexts| contexts.into_iter().next())
        .map(|ctx| std::path::PathBuf::from(ctx.working_directory))
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
    async fn initialize_advertises_session_list_capability() {
        let agent = test_agent();
        let request = InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1);

        let response = agent.initialize(request).await.unwrap();

        assert_eq!(
            response.agent_capabilities,
            AgentCapabilities::new()
                .load_session(true)
                .session_capabilities(
                    SessionCapabilities::new()
                        .list(SessionListCapabilities::new())
                        .resume(SessionResumeCapabilities::new())
                        .close(SessionCloseCapabilities::new())
                )
        );
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
