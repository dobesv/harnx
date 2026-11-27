//! Shared harness for the native-mode (per-user credentials) integration
//! tests: a per-user mock upstream MCP server over Streamable HTTP, the
//! NATS-backed `NativeServer`, and connect-page HTTP helpers.
//!
//! The token markers below are the stored secrets. They never appear in any
//! expected tool result or stored plaintext value; the secret-marker scans in
//! the tests assert exactly that.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, DiscoverResult, ErrorData, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerConfig,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde_json::{json, Map, Value};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use clap::Parser;
use harnx_mcp_remote::cli::Cli;
use harnx_mcp_remote::native::{NativeConfig, NativeServer};
use harnx_toolset::Toolset;

pub const ALICE_TOKEN: &str = "alice-secret-token-7f3a42-marker";
pub const BOB_TOKEN: &str = "bob-secret-token-9b21cc-marker";
pub const OWNERLESS_TOKEN: &str = "solo-secret-token-31d77e-marker";

pub const ALICE_LABEL: &str = "alice@example.test";
pub const BOB_LABEL: &str = "bob@example.test";
pub const OWNERLESS_LABEL: &str = "solo@example.test";

/// A stored token credential for pool-level tests: only the material
/// matters to the pool, so the bookkeeping fields are filled minimally.
pub fn token_record(
    connection_id: &str,
    token: &str,
) -> harnx_credentials::Versioned<harnx_credentials::CredentialRecord> {
    token_record_revised(connection_id, token, 1)
}

/// A token credential with an explicit store revision, so a test can model
/// a reconnect that bumped the credential's revision.
pub fn token_record_revised(
    connection_id: &str,
    token: &str,
    revision: u64,
) -> harnx_credentials::Versioned<harnx_credentials::CredentialRecord> {
    let now = chrono::Utc::now();
    harnx_credentials::Versioned {
        revision,
        record: harnx_credentials::CredentialRecord {
            schema_version: 1,
            owner_user_id: None,
            server: "native-test".into(),
            created_at: now,
            updated_at: now,
            expires_at: now + chrono::TimeDelta::days(30),
            status: harnx_credentials::CredentialStatus::Active,
            account_label: None,
            connection_id: connection_id.into(),
            material: harnx_credentials::CredentialMaterial::Token(
                harnx_credentials::TokenMaterial {
                    token: harnx_credentials::Secret::new(token.to_string()),
                    header: "authorization".into(),
                    scheme: None,
                },
            ),
        },
    }
}

/// The per-user tool lists the mock upstream serves for each token.
fn default_user_tools(reserved_collision: bool) -> Vec<String> {
    let mut tools = vec!["whoami".to_string(), "search".to_string()];
    if reserved_collision {
        // An upstream tool shadowing the reserved management raw name.
        tools.push("connect".to_string());
    }
    tools
}

/// Everything the mock upstream knows: token -> (label, tools).
type UpstreamUsersMap = HashMap<String, (String, Vec<String>)>;

#[derive(Clone, Default)]
pub struct UpstreamUsers {
    map: Arc<Mutex<UpstreamUsersMap>>,
    header: Arc<Mutex<String>>,
}

impl UpstreamUsers {
    fn label(&self, token: &str) -> Option<String> {
        self.map
            .lock()
            .expect("upstream users lock")
            .get(token)
            .map(|(label, _)| label.clone())
    }

    fn tools(&self, token: &str) -> Option<Vec<String>> {
        self.map
            .lock()
            .expect("upstream users lock")
            .get(token)
            .map(|(_, tools)| tools.clone())
    }

    /// Change one user's tool list, for `list_changed` tests.
    pub fn set_tools(&self, token: &str, tools: Vec<String>) {
        let mut map = self.map.lock().expect("upstream users lock");
        let entry = map
            .entry(token.to_string())
            .or_insert_with(|| (format!("{token}-label"), default_user_tools(false)));
        entry.1 = tools;
    }

    /// The presented credential: the configured header's value, with a
    /// `Bearer ` scheme prefix stripped when present.
    fn presented(&self, context: &RequestContext<RoleServer>) -> Option<String> {
        drop(self.map.lock().expect("users lock"));
        let header = self.header.lock().expect("header lock").clone();
        context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.headers.get(&header))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .strip_prefix("Bearer ")
                    .map(str::to_string)
                    .unwrap_or(value)
            })
    }
}

// qual:allow(srp, god_struct) reason: "test fixture: one mock server whose per-user tools and handshake bookkeeping share all its state"
#[derive(Clone)]
struct PerUserUpstream {
    users: UpstreamUsers,
    initialize_count: Arc<AtomicUsize>,
    peers: Arc<Mutex<Vec<Peer<RoleServer>>>>,
    /// When set, the next handshake blocks on the gate until the test
    /// releases it (a stalled upstream connect).
    gate: Option<Arc<HandshakeGate>>,
    /// When set, `tools/list` answers with an error that echoes the
    /// presented credential (a provider reflecting the secret).
    fail_list_tools: Arc<AtomicBool>,
    /// When set, both handshake methods answer with an error that echoes the
    /// presented credential: a provider rejecting the connect while naming
    /// the credential it was given.
    fail_handshake: Arc<AtomicBool>,
    /// When set, the server is legacy-only: `server/discover` is refused so
    /// the client must fall back to the sessionful `initialize` handshake
    /// (live server-side sessions).
    legacy_sessions: bool,
    /// The raw `harnx:structured_result` `_meta` value of each
    /// `structured_echo` call — `None` when the key is absent — in call
    /// order, separate from the response-shape decision.
    structured_requests: Arc<Mutex<Vec<Option<Value>>>>,
}

/// A deterministic handshake stall: once armed, the next upstream handshake
/// blocks until the test releases it. A two-party barrier keeps both sides
/// synchronized without sleeps.
pub struct HandshakeGate {
    armed: AtomicBool,
    barrier: tokio::sync::Barrier,
}

impl HandshakeGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(false),
            barrier: tokio::sync::Barrier::new(2),
        })
    }

    /// The next handshake blocks until [`Self::release`] meets it.
    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    async fn wait_if_armed(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.barrier.wait().await;
        }
    }

    /// Let the armed handshake through.
    pub async fn release(&self) {
        self.barrier.wait().await;
    }
}

/// Argument shape for the mock upstream's tools: a typed (empty) object. The
/// worker always sends `args: {}` for these; a bare `Parameters<Value>` would
/// panic server-side because schemars gives `Value` a non-object root schema.
#[derive(Debug, Clone, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct UpstreamArgs {}

#[tool_router]
impl PerUserUpstream {
    #[tool(description = "Report the connected user's label")]
    async fn whoami(
        &self,
        _args: Parameters<UpstreamArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.label(&context)
            .map(|label| CallToolResult::success(vec![ContentBlock::text(format!("user:{label}"))]))
            .ok_or_else(|| ErrorData::internal_error("no known credential presented", None))
    }

    #[tool(description = "A per-user tool")]
    async fn search(
        &self,
        _args: Parameters<UpstreamArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.label(&context)
            .map(|label| {
                CallToolResult::success(vec![ContentBlock::text(format!("search:{label}"))])
            })
            .ok_or_else(|| ErrorData::internal_error("no known credential presented", None))
    }

    /// A provider that echoes the submitted credential back in a
    /// JSON-RPC error.
    #[tool(description = "Echo the presented credential in a JSON-RPC error")]
    async fn leak_error(
        &self,
        _args: Parameters<UpstreamArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let token = self.users.presented(&context).unwrap_or_default();
        Err(ErrorData::internal_error(
            format!("presented token rejected: {token}"),
            None,
        ))
    }

    /// A provider that echoes the submitted credential back in tool
    /// result content.
    #[tool(description = "Echo the presented credential in a result")]
    async fn leak_result(
        &self,
        _args: Parameters<UpstreamArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let token = self.users.presented(&context).unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "echo:{token}"
        ))]))
    }

    /// An upstream that keys its result shape on the structured-result
    /// request: a script-style `structuredContent` when asked, model-style
    /// text otherwise.
    #[tool(description = "Report script-style streams when the structured result is requested")]
    async fn structured_echo(
        &self,
        _args: Parameters<UpstreamArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let requested = context
            .meta
            .get(harnx_toolset::CAPABILITY_STRUCTURED_RESULT)
            .cloned();
        self.structured_requests
            .lock()
            .expect("structured request log lock")
            .push(requested.clone());
        if requested.as_ref().and_then(Value::as_bool).unwrap_or(false) {
            let mut result =
                CallToolResult::success(vec![ContentBlock::text("structured mode content")]);
            result.structured_content = Some(json!({
                "stdout": "upstream stdout\n",
                "stderr": "upstream stderr",
                "exit_code": 3,
                "items": ["alpha", "beta"],
                "count": 2,
            }));
            Ok(result)
        } else {
            Ok(CallToolResult::success(vec![ContentBlock::text(
                "model-style text",
            )]))
        }
    }

    fn label(&self, context: &RequestContext<RoleServer>) -> Option<String> {
        self.users
            .presented(context)
            .as_deref()
            .and_then(|token| self.users.label(token))
    }

    /// The credential-echoing handshake rejection, while the flag is set:
    /// a provider that fails the connect while naming the presented token.
    fn handshake_echo_error(&self, context: &RequestContext<RoleServer>) -> Option<ErrorData> {
        if self.fail_handshake.load(Ordering::SeqCst) {
            let token = self.users.presented(context).unwrap_or_default();
            return Some(ErrorData::internal_error(
                format!("handshake rejected for credential {token}"),
                None,
            ));
        }
        None
    }
}

#[tool_handler]
impl ServerHandler for PerUserUpstream {
    /// Count one "handshake" per upstream session. rmcp's Auto client
    /// lifecycle negotiates the discover-era 2026-07-28 revision when the
    /// upstream supports it, so sessions may arrive here instead of through
    /// `initialize` — incrementing in both keeps the counter meaning "one
    /// upstream session opened".
    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        if let Some(error) = self.handshake_echo_error(&context) {
            return Err(error);
        }
        if let Some(gate) = &self.gate {
            gate.wait_if_armed().await;
        }
        // Legacy-only servers have no discover; rmcp's Auto lifecycle takes
        // the error as the signal to fall back to `initialize`.
        if self.legacy_sessions {
            return Err(ErrorData::internal_error("legacy server", None));
        }
        self.initialize_count.fetch_add(1, Ordering::SeqCst);
        self.peers
            .lock()
            .expect("peer lock")
            .push(context.peer.clone());
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        ))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        if let Some(error) = self.handshake_echo_error(&context) {
            return Err(error);
        }
        if let Some(gate) = &self.gate {
            gate.wait_if_armed().await;
        }
        self.initialize_count.fetch_add(1, Ordering::SeqCst);
        self.peers
            .lock()
            .expect("peer lock")
            .push(context.peer.clone());
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let Some(tools) = self
            .users
            .presented(&context)
            .as_deref()
            .and_then(|token| self.users.tools(token))
        else {
            return Err(ErrorData::internal_error(
                "no known credential presented",
                None,
            ));
        };
        // A credential-echoing error while the flag is set.
        if self.fail_list_tools.load(Ordering::SeqCst) {
            let token = self.users.presented(&context).unwrap_or_default();
            return Err(ErrorData::internal_error(
                format!("token {token} may not list tools"),
                None,
            ));
        }
        let tools = tools
            .into_iter()
            .map(|name| {
                // The script-output mapping rides in `_meta`, the way a
                // script-capable upstream advertises it.
                let meta = (name == "structured_echo").then(|| {
                    let output = harnx_toolset::ScriptOutput {
                        stdout: Some("stdout".to_string()),
                        stderr: Some("stderr".to_string()),
                        exit_code: Some("exit_code".to_string()),
                        summary: Some("mapped stdout/stderr/exit_code, like a command".to_string()),
                        ..Default::default()
                    };
                    let mut meta = rmcp::model::MetaObject::default();
                    meta.0.insert(
                        harnx_toolset::SCRIPT_OUTPUT_META_KEY.to_string(),
                        serde_json::to_value(output).expect("script output serializes"),
                    );
                    meta
                });
                let mut tool = rmcp::model::Tool::new(
                    name,
                    "per-user upstream tool",
                    Arc::new(serde_json::Map::new()),
                );
                tool.meta = meta;
                tool
            })
            .collect();
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        );
        info.server_info = rmcp::model::Implementation::new("per-user-upstream", "1.0.0");
        info
    }
}

// qual:allow(srp, god_struct) reason: "test-fixture handle: small independent knobs over one spawned mock server"
pub struct UpstreamHandle {
    pub url: String,
    pub users: UpstreamUsers,
    pub initialize_count: Arc<AtomicUsize>,
    fail_list_tools: Arc<AtomicBool>,
    fail_handshake: Arc<AtomicBool>,
    /// The raw `harnx:structured_result` `_meta` value of each
    /// `structured_echo` call — `None` when the key is absent — in call
    /// order.
    structured_requests: Arc<Mutex<Vec<Option<Value>>>>,
    /// The in-memory session table of a session-mode upstream.
    pub session_manager:
        Option<Arc<rmcp::transport::streamable_http_server::session::local::LocalSessionManager>>,
    shutdown: CancellationToken,
    join: tokio::task::JoinHandle<()>,
    peers: Arc<Mutex<Vec<Peer<RoleServer>>>>,
}

impl UpstreamHandle {
    /// The raw `harnx:structured_result` `_meta` value of each
    /// `structured_echo` call (`None` when absent), in call order.
    pub fn structured_requests(&self) -> Vec<Option<Value>> {
        self.structured_requests
            .lock()
            .expect("structured request log lock")
            .clone()
    }
}

impl UpstreamHandle {
    /// Push `notifications/tools/list_changed` to every connected client.
    pub async fn notify_list_changed(&self) {
        let peers: Vec<Peer<RoleServer>> = self.peers.lock().expect("peer lock").clone();
        for peer in peers {
            let _ = peer.notify_tool_list_changed().await;
        }
    }

    /// Make `tools/list` answer with a credential-echoing error.
    pub fn set_fail_list_tools(&self, fail: bool) {
        self.fail_list_tools.store(fail, Ordering::SeqCst);
    }

    /// Make both handshake methods answer with a credential-echoing error.
    pub fn set_fail_handshake(&self, fail: bool) {
        self.fail_handshake.store(fail, Ordering::SeqCst);
    }

    pub async fn shutdown(self) {
        self.peers.lock().expect("peer lock").clear();
        self.shutdown.cancel();
        let _ = self.join.await;
    }
}

/// Everything a spawned mock upstream can vary on.
struct UpstreamOptions<'a> {
    token_header: &'a str,
    reserved_collision: bool,
    gate: Option<Arc<HandshakeGate>>,
    /// Give alice the credential-echoing leak tools.
    leak_tools: bool,
    /// Serve real `Mcp-Session-Id` sessions with an observable in-memory
    /// session table, so a client's shutdown DELETE is visible.
    session_mode: bool,
    /// Give alice the shape-switching `structured_echo` tool.
    structured_tools: bool,
}

/// Spawn the per-user mock upstream. `token_header` is the HTTP header the
/// native server sends the token in (it must match the test's CLI flags).
pub async fn spawn_per_user_upstream(
    token_header: &str,
    reserved_collision: bool,
) -> Result<UpstreamHandle> {
    spawn_upstream_with(UpstreamOptions {
        token_header,
        reserved_collision,
        gate: None,
        leak_tools: false,
        session_mode: false,
        structured_tools: false,
    })
    .await
}

/// A mock upstream whose next handshake stalls until released.
pub async fn spawn_gated_upstream(
    token_header: &str,
    gate: Arc<HandshakeGate>,
) -> Result<UpstreamHandle> {
    spawn_upstream_with(UpstreamOptions {
        token_header,
        reserved_collision: false,
        gate: Some(gate),
        leak_tools: false,
        session_mode: false,
        structured_tools: false,
    })
    .await
}

/// A mock upstream whose users also see the credential-echoing leak tools.
pub async fn spawn_leaky_upstream(token_header: &str) -> Result<UpstreamHandle> {
    spawn_upstream_with(UpstreamOptions {
        token_header,
        reserved_collision: false,
        gate: None,
        leak_tools: true,
        session_mode: false,
        structured_tools: false,
    })
    .await
}

/// A mock upstream whose alice sees the shape-switching `structured_echo`
/// tool: script-style `structuredContent` when the structured result is
/// requested, model-style text otherwise.
pub async fn spawn_structured_upstream(token_header: &str) -> Result<UpstreamHandle> {
    spawn_upstream_with(UpstreamOptions {
        token_header,
        reserved_collision: false,
        gate: None,
        leak_tools: false,
        session_mode: false,
        structured_tools: true,
    })
    .await
}

/// A session-mode mock upstream: legacy sessions over an in-memory session
/// manager, so the test can see live sessions and their termination. The
/// manager is returned alongside the handle.
pub async fn spawn_session_upstream(
    token_header: &str,
) -> Result<(
    UpstreamHandle,
    Arc<rmcp::transport::streamable_http_server::session::local::LocalSessionManager>,
)> {
    let handle = spawn_upstream_with(UpstreamOptions {
        token_header,
        reserved_collision: false,
        gate: None,
        leak_tools: false,
        session_mode: true,
        structured_tools: false,
    })
    .await?;
    let manager = handle
        .session_manager
        .as_ref()
        .expect("session-mode upstream keeps its manager")
        .clone();
    Ok((handle, manager))
}

/// A session-mode mock upstream whose next handshake stalls until released:
/// the fence and session-leak tests need one parked stale connect alongside
/// an observable session table.
pub async fn spawn_gated_session_upstream(
    token_header: &str,
    gate: Arc<HandshakeGate>,
) -> Result<(
    UpstreamHandle,
    Arc<rmcp::transport::streamable_http_server::session::local::LocalSessionManager>,
)> {
    let handle = spawn_upstream_with(UpstreamOptions {
        token_header,
        reserved_collision: false,
        gate: Some(gate),
        leak_tools: false,
        session_mode: true,
        structured_tools: false,
    })
    .await?;
    let manager = handle
        .session_manager
        .as_ref()
        .expect("session-mode upstream keeps its manager")
        .clone();
    Ok((handle, manager))
}

async fn spawn_upstream_with(options: UpstreamOptions<'_>) -> Result<UpstreamHandle> {
    let UpstreamOptions {
        token_header,
        reserved_collision,
        gate,
        leak_tools,
        session_mode,
        structured_tools,
    } = options;
    let users = UpstreamUsers::default();
    {
        let mut map = users.map.lock().expect("users lock");
        let alice_tools = match (reserved_collision, leak_tools, structured_tools) {
            (_, true, _) => vec![
                "whoami".to_string(),
                "search".to_string(),
                "leak_error".to_string(),
                "leak_result".to_string(),
            ],
            (true, false, _) => default_user_tools(true),
            (false, false, true) => vec!["structured_echo".to_string()],
            (false, false, false) => default_user_tools(false),
        };
        map.insert(
            ALICE_TOKEN.to_string(),
            (ALICE_LABEL.to_string(), alice_tools),
        );
        map.insert(
            BOB_TOKEN.to_string(),
            (BOB_LABEL.to_string(), default_user_tools(false)),
        );
        map.insert(
            OWNERLESS_TOKEN.to_string(),
            (OWNERLESS_LABEL.to_string(), default_user_tools(false)),
        );
    }
    *users.header.lock().expect("header lock") = token_header.to_string();
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let peers = Arc::new(Mutex::new(Vec::new()));
    let fail_list_tools = Arc::new(AtomicBool::new(false));
    let fail_handshake = Arc::new(AtomicBool::new(false));
    let structured_requests = Arc::new(Mutex::new(Vec::new()));
    let server = PerUserUpstream {
        users: users.clone(),
        initialize_count: Arc::clone(&initialize_count),
        peers: Arc::clone(&peers),
        gate,
        fail_list_tools: Arc::clone(&fail_list_tools),
        fail_handshake: Arc::clone(&fail_handshake),
        legacy_sessions: session_mode,
        structured_requests: Arc::clone(&structured_requests),
    };
    let shutdown = CancellationToken::new();
    let mut config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_cancellation_token(shutdown.child_token())
        .disable_allowed_hosts();
    let session_manager: Option<
        Arc<rmcp::transport::streamable_http_server::session::local::LocalSessionManager>,
    > = if session_mode {
        config = config.with_legacy_session_mode(true);
        Some(Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        ))
    } else {
        config = config.with_legacy_session_mode(false);
        None
    };
    let app = match &session_manager {
        Some(manager) => {
            let manager = manager.clone();
            let mcp_service =
                StreamableHttpService::new(move || Ok(server.clone()), manager, config);
            axum::Router::new().nest_service("/mcp", mcp_service)
        }
        None => {
            let mcp_service = StreamableHttpService::new(
                move || Ok(server.clone()),
                Arc::new(NeverSessionManager::default()),
                config,
            );
            axum::Router::new().nest_service("/mcp", mcp_service)
        }
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let (port, join) =
        crate::common::serve_test_app(listener, app, shutdown.clone(), "per-user upstream")?;
    Ok(UpstreamHandle {
        url: format!("http://127.0.0.1:{port}/mcp"),
        users,
        initialize_count,
        fail_list_tools,
        fail_handshake,
        structured_requests,
        shutdown,
        join,
        peers,
        session_manager,
    })
}

/// One native-mode fixture: a NATS server, a key file and a started
/// `NativeServer` against the given upstream URL.
pub struct NativeFixture {
    pub server: NativeServer,
    /// Held so the spawned NATS server stays up for the fixture's lifetime.
    #[allow(dead_code)]
    pub nats: harnx_test_bins::NatsServerHandle,
    pub client: async_nats::Client,
    /// The base URL of the connect pages as actually reachable locally.
    pub http_base: String,
    /// Held to keep the instance's background loops owned by the fixture.
    #[allow(dead_code)]
    pub shutdown: CancellationToken,
}

/// Build the native CLI args every fixture shares, then apply `extra`. An
/// `extra` that repeats a shared flag (e.g. a test's own `--http-bind`)
/// replaces the default instead of colliding with it.
pub fn native_cli(upstream_url: &str, key_file: &std::path::Path, extra: &[&str]) -> Cli {
    let mut args = vec![
        "harnx-mcp-remote",
        "--url",
        upstream_url,
        "--credentials-key-file",
        key_file.to_str().expect("utf-8 key file path"),
        "--name",
        "jira",
        "--auth-methods",
        "token",
        "--http-bind",
        "127.0.0.1:0",
    ];
    let overridden: Vec<&str> = extra
        .windows(2)
        .filter(|pair| pair[0].starts_with("--"))
        .map(|pair| pair[0])
        .collect();
    let mut i = 0;
    while i < args.len() {
        if overridden.contains(&args[i]) && i + 1 < args.len() {
            args.drain(i..i + 2);
        } else {
            i += 1;
        }
    }
    args.extend(extra.iter().copied());
    Cli::parse_from(args)
}

/// Write a key file the server can load (same format `Keyring::load` reads).
pub fn write_key_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let json = json!({
        "active": "k1",
        "keys": { "k1": STANDARD.encode([0x5bu8; 32]) }
    });
    let path = dir.path().join("credentials-key.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json).expect("serialize key file"),
    )
    .expect("write key file");
    path
}

/// Start a `NativeServer` against a freshly spawned NATS server. The scope and
/// identity are fixed per-fixture so both instances of a multi-instance test
/// share the registry and tool subject.
pub async fn start_native(config: NativeConfig) -> Result<NativeFixture> {
    let nats = harnx_test_bins::spawn_nats_server()
        .await?
        .context("NATS tests require nats-server; set NATS_SERVER_BIN (no silent skips)")?;
    let client = async_nats::connect(nats.url()).await?;
    let shutdown = CancellationToken::new();
    let server = start_native_on(config, &client, shutdown.clone()).await?;
    let http_base = format!("http://{}", server.local_addr);
    Ok(NativeFixture {
        server,
        nats,
        client,
        http_base,
        shutdown,
    })
}

/// Start a `NativeServer` on an existing NATS connection, for multi-instance
/// tests: every instance over one client shares the flows, catalogs and the
/// registry.
pub async fn start_native_on(
    config: NativeConfig,
    client: &async_nats::Client,
    shutdown: CancellationToken,
) -> Result<NativeServer> {
    let scope = harnx_core::instance::ServerScope::from_string(format!(
        "native-test-{}",
        std::process::id()
    ));
    let identity = harnx_toolset_server::RegistrationIdentity::default();
    NativeServer::start(
        config,
        harnx_nats_common::connect::NatsConnection {
            client: client.clone(),
            replicas: 1,
        },
        scope,
        identity,
        harnx_mcp_remote::native::NativeServerRuntime {
            shutdown,
            refresh_interval: Duration::from_millis(100),
        },
    )
    .await
}

/// Invoke one tool on the toolset for a principal, with optional challenge
/// answers and request state — the shape the worker sends.
pub async fn invoke(
    toolset: &harnx_mcp_remote::native::toolset::RemoteCredentialsToolset,
    tool: &str,
    user_id: Option<&str>,
    responses: &[(String, harnx_toolset::ToolInputResponse)],
    request_state: Value,
) -> Result<Value, harnx_toolset::ToolInvokeError> {
    let context = harnx_toolset::ToolInvocationContext {
        user_id: user_id.map(str::to_string),
        input_responses: responses.iter().cloned().collect(),
        request_state,
        ..Default::default()
    };
    let invocation = harnx_toolset::ToolInvocation {
        tool: tool.to_string(),
        args: json!({}),
        context,
        cancel: CancellationToken::new(),
    };
    toolset.invoke_with_context(invocation).await
}

/// Invoke one tool with explicit invocation capabilities — the shape a
/// script-mode inner call sends when it asks for a structured result.
pub async fn invoke_with_capabilities(
    toolset: &harnx_mcp_remote::native::toolset::RemoteCredentialsToolset,
    tool: &str,
    user_id: Option<&str>,
    capabilities: &[&str],
) -> Result<Value, harnx_toolset::ToolInvokeError> {
    let context = harnx_toolset::ToolInvocationContext {
        user_id: user_id.map(str::to_string),
        capabilities: capabilities.iter().map(|name| name.to_string()).collect(),
        ..Default::default()
    };
    let invocation = harnx_toolset::ToolInvocation {
        tool: tool.to_string(),
        args: json!({}),
        context,
        cancel: CancellationToken::new(),
    };
    toolset.invoke_with_context(invocation).await
}

/// Pull the `InputRequired` challenge out of an error, with its request state.
pub fn url_challenge(
    error: harnx_toolset::ToolInvokeError,
) -> (harnx_toolset::ToolInputRequired, Value) {
    match error {
        harnx_toolset::ToolInvokeError::InputRequired(input) => {
            let state = input.request_state.clone();
            (*input, state)
        }
        other => panic!("expected an input_required challenge, got: {other:?}"),
    }
}

pub fn continue_response() -> harnx_toolset::ToolInputResponse {
    harnx_toolset::ToolInputResponse {
        action: harnx_core::tool::ToolInputRequired::URL_ACTION_CONTINUE.to_string(),
        data: Map::new(),
        note: None,
    }
}

pub fn cancel_response() -> harnx_toolset::ToolInputResponse {
    harnx_toolset::ToolInputResponse {
        action: harnx_core::tool::ToolInputRequired::URL_ACTION_CANCEL.to_string(),
        data: Map::new(),
        note: None,
    }
}

/// The challenge key of one round, as `Toolset::connect` builds it.
pub fn connect_challenge_key(round: u64) -> String {
    format!("connect-round-{round}")
}

/// Complete one token connect flow over the real HTTP pages: fetch the page,
/// post the token form, and return the response status. `extra_headers`
/// travel on both requests (the trusted-user header tests).
pub async fn complete_token_form(
    http_base: &str,
    challenge_id: &str,
    token: &str,
    expires_at: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> Result<(u16, String)> {
    let http = reqwest::Client::builder().build()?;
    let page_request = http.get(format!("{http_base}/connect/{challenge_id}"));
    let page_request = apply_extra_headers(page_request, extra_headers);
    let page = page_request.send().await?;
    let page_status = page.status().as_u16();
    let html = page.text().await?;
    if page_status != 200 {
        return Ok((page_status, html));
    }
    let csrf = extract_csrf(&html).expect("csrf token on the page");
    let form = TokenForm::new(token, &csrf).with_expires(expires_at);
    post_token_form(http_base, challenge_id, form, extra_headers).await
}

pub struct TokenForm<'a> {
    pub token: &'a str,
    pub csrf: &'a str,
    pub expires_at: Option<&'a str>,
}

impl<'a> TokenForm<'a> {
    pub fn new(token: &'a str, csrf: &'a str) -> Self {
        Self {
            token,
            csrf,
            expires_at: None,
        }
    }

    pub fn with_expires(mut self, expires_at: Option<&'a str>) -> Self {
        self.expires_at = expires_at;
        self
    }
}

fn apply_extra_headers(
    mut req: reqwest::RequestBuilder,
    headers: &[(&str, &str)],
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    req
}

/// Post the token form directly (no page fetch), for replay and race tests
/// where the page may already refuse.
pub async fn post_token_form(
    http_base: &str,
    challenge_id: &str,
    form: TokenForm<'_>,
    extra_headers: &[(&str, &str)],
) -> Result<(u16, String)> {
    let http = reqwest::Client::builder().build()?;
    let mut params = vec![
        ("csrf_token", form.csrf.to_string()),
        ("token", form.token.to_string()),
    ];
    if let Some(expires_at) = form.expires_at {
        params.push(("expires_at", expires_at.to_string()));
    }
    let request = http
        .post(format!("{http_base}/connect/{challenge_id}/token"))
        .form(&params);
    let request = apply_extra_headers(request, extra_headers);
    let response = request.send().await?;
    let status = response.status().as_u16();
    let body = response.text().await?;
    Ok((status, body))
}

/// Read the CSRF hidden field out of the connect page.
pub fn extract_csrf(html: &str) -> Option<String> {
    let marker = "name=\"csrf_token\" value=\"";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The stored registration for one audience, or `None`.
pub async fn stored_registration(
    handle: &harnx_toolset_server::RegistrationHandle,
    audience: &harnx_toolset_server::HandleAudience,
) -> Option<harnx_toolset_server::StoredRegistration> {
    handle.read(audience).await.ok().flatten()
}

/// A `NativeConfig` from CLI args, panicking on validation errors so test
/// failures read as the validation message.
pub fn config_from_cli(cli: &Cli) -> NativeConfig {
    NativeConfig::from_cli(cli).unwrap_or_else(|error| panic!("{}", error))
}

// Background refresh can advance the revision even after shutdown is cancelled.
// Retry conflicts without weakening the connection check or expected delete outcome.
pub async fn delete_registration_checked(
    handle: &harnx_toolset_server::RegistrationHandle,
    audience: &harnx_toolset_server::HandleAudience,
    expected_connection: &str,
    deleted_connection: &str,
) -> harnx_toolset_server::ConditionalDelete {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let stored = stored_registration(handle, audience)
            .await
            .expect("read registration before delete");
        assert_eq!(
            stored.registration.connection_id.as_deref(),
            Some(expected_connection),
            "registration connection_id must match expected"
        );
        match handle
            .delete(audience, stored.revision, deleted_connection)
            .await
        {
            Ok(outcome) => return outcome,
            Err(harnx_toolset_server::RegistrationHandleError::Conflict) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out retrying registration delete on conflict"
                );
                tokio::task::yield_now().await;
            }
            Err(_) => panic!("unexpected error deleting registration"),
        }
    }
}
