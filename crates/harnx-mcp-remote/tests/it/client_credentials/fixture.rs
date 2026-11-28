//! Shared fixture for client-credentials proxy integration tests: one axum
//! upstream hosting the token endpoint, an auth-guarded Streamable-HTTP MCP
//! server and a redirect leak target, with counters for every contract the
//! T3 acceptance needs.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, post};
use axum::Router;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, InitializeRequestParams, InitializeResult,
    ProtocolVersion, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::session::{
    never::NeverSessionManager, SessionManager,
};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde_json::json;
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::RootCertStore;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::common::tool_args;

pub const CLIENT_ID: &str = "svc-account";
pub const CLIENT_SECRET: &str = "super-secret-value";
/// Tokens this fixture issues, in order: `access-token-1`, `access-token-2`, …
pub const TOKEN_PREFIX: &str = "access-token-";
pub const CUSTOM_HEADER: &str = "X-Custom";
pub const CUSTOM_HEADER_VALUE: &str = "proxy-hello";
/// Separate sentinels the failing initialize handler places in its JSON-RPC
/// error message and structured data; the OAuth outward boundary must drop
/// both (B1/B9).
pub const INIT_MSG_SENTINEL: &str = "init-msg-sentinel-4101";
pub const INIT_DATA_SENTINEL: &str = "init-data-sentinel-4102";

#[derive(Debug, Clone)]
pub enum TokenMode {
    /// Issue a fresh token with the configured `expires_in`.
    Issue,
    /// Answer every exchange with a status/code error whose description
    /// carries provider prose that must never reach a client.
    Error { status: u16, code: &'static str },
    /// Answer every exchange with a 307 to the leak target.
    Redirect,
}

/// The static safe text the proxy substitutes for failed tool results and
/// decoded MCP service errors in client-credentials mode.
pub const SAFE_UPSTREAM_FAILURE: &str =
    "remote MCP call failed; upstream error output was withheld";

// qual:allow(srp, god_struct) reason: "One shared test fixture state: every counter and token set backs the same upstream, and the client-credentials assertions read them together after each call; splitting per-method state would scatter the exact contracts the tests assert on."
pub struct UpstreamState {
    pub token_mode: Mutex<TokenMode>,
    pub expires_in: u64,
    /// When set, every issued token is immediately rejected by the MCP
    /// guard: one 401 replay, then the call must stop.
    pub auto_reject_new_tokens: AtomicBool,
    /// When set, the MCP guard answers any POST carrying a session id with
    /// 404, the way a server that lost the session does. Only meaningful on
    /// the stateful spawn, which issues session ids at all.
    pub expire_sessions: AtomicBool,
    /// When set, the upstream `initialize` handler (and every automatic
    /// session re-initialization) answers with a JSON-RPC error echoing the
    /// presented bearer token in both message and structured data.
    pub fail_initialize: AtomicBool,
    pub valid_tokens: Mutex<HashSet<String>>,
    pub rejected_tokens: Mutex<HashSet<String>>,
    pub simulate_403: AtomicBool,
    /// Once `mcp_requests` passes this threshold, POSTs are answered with a
    /// 307 to the leak target (usize::MAX disables).
    pub redirect_mcp_after: AtomicUsize,
    pub token_exchanges: AtomicUsize,
    pub unauthorized_401s: AtomicUsize,
    pub mcp_requests: AtomicUsize,
    pub leak_hits: AtomicUsize,
    pub token_saw_custom_header: AtomicBool,
    pub token_forms: Mutex<Vec<Vec<(String, String)>>>,
    /// First bearer token the MCP endpoint ever accepted (for retired-token
    /// echo tests).
    pub first_accepted_token: Mutex<Option<String>>,
    pub last_authorization: Mutex<Option<String>>,
    pub initialize_count: AtomicUsize,
}

impl UpstreamState {
    /// Issues `access-token-<n>` and returns it.
    fn issue_token(&self) -> String {
        let index = self.token_exchanges.fetch_add(1, Ordering::SeqCst);
        let token = format!("{TOKEN_PREFIX}{}", index + 1);
        self.valid_tokens.lock().unwrap().insert(token.clone());
        if self.auto_reject_new_tokens.load(Ordering::SeqCst) {
            self.rejected_tokens.lock().unwrap().insert(token.clone());
        }
        token
    }

    pub fn reject_token(&self, token: &str) {
        self.rejected_tokens
            .lock()
            .unwrap()
            .insert(token.to_string());
    }

    pub fn recorded_token_form(&self, index: usize) -> Vec<(String, String)> {
        self.token_forms
            .lock()
            .unwrap()
            .get(index)
            .expect("token form recorded")
            .clone()
    }
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct EchoArgs {
    pub text: String,
}

#[derive(Clone)]
// qual:allow(srp, god_struct) reason: "The upstream test handler owns echo plus the failure surfaces (JSON-RPC error, is_error result, retired-token echo); one handler keeps the tool set and the recorded token state in one place for the sanitization assertions."
pub struct CcHandler {
    pub state: Arc<UpstreamState>,
    pub include_headers: bool,
    /// Negotiate a pre-2026-07-28 protocol version. Sessions were removed
    /// from the 2026-07-28 revision (SEP-2567), so only a legacy version
    /// makes the upstream issue real session ids for the stateful tests.
    pub legacy_protocol: bool,
}

fn presented_authorization(context: &RequestContext<RoleServer>) -> Option<String> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.headers.get(axum::http::header::AUTHORIZATION))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[tool_router]
impl CcHandler {
    fn token_text(&self, context: &RequestContext<RoleServer>) -> String {
        presented_authorization(context)
            .or_else(|| self.state.last_authorization.lock().unwrap().clone())
            .unwrap_or_default()
    }

    #[tool(description = "Echo the input text, optionally with request headers")]
    async fn echo(
        &self,
        Parameters(args): Parameters<EchoArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut payload = json!({ "text": args.text });
        if self.include_headers {
            let headers = context
                .extensions
                .get::<axum::http::request::Parts>()
                .map(|parts| headers_to_json(&parts.headers))
                .unwrap_or_default();
            payload["headers"] = headers;
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }

    #[tool(description = "Fail with a JSON-RPC error echoing the token it saw")]
    async fn fail_jsonrpc(
        &self,
        _args: Parameters<EchoArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Err(ErrorData::internal_error(
            format!(
                "upstream rejected: saw bearer {}",
                self.token_text(&context)
            ),
            None,
        ))
    }

    #[tool(description = "Fail with an is_error tool result echoing the token it saw")]
    async fn fail_is_error(
        &self,
        _args: Parameters<EchoArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "tool failed after using bearer {}",
            self.token_text(&context)
        ))]))
    }

    #[tool(description = "Fail with a JSON-RPC error echoing the first (retired) token")]
    async fn fail_with_retired_token(
        &self,
        _args: Parameters<EchoArgs>,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let first = self
            .state
            .first_accepted_token
            .lock()
            .unwrap()
            .clone()
            .expect("a token was accepted earlier in the test");
        Err(ErrorData::internal_error(
            format!("stale bearer {first} was rejected here"),
            None,
        ))
    }
}

fn headers_to_json(headers: &axum::http::HeaderMap) -> serde_json::Value {
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in headers {
        grouped
            .entry(name.as_str().to_ascii_lowercase())
            .or_default()
            .push(value.to_str().unwrap_or_default().to_string());
    }
    // Match common.rs: a single value renders as a scalar.
    let single = |values: Vec<String>| -> serde_json::Value {
        match values.len() {
            1 => values.into_iter().next().expect("one value").into(),
            _ => values.into(),
        }
    };
    grouped
        .into_iter()
        .map(|(key, values)| (key, single(values)))
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

#[tool_handler]
impl ServerHandler for CcHandler {
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        self.state.initialize_count.fetch_add(1, Ordering::SeqCst);
        if self.state.fail_initialize.load(Ordering::SeqCst) {
            // Echo the presented credential in the message and in structured
            // data, so the outward-boundary tests have two distinct sentinels
            // to trace (B1 re-initialization, B9 first initialize).
            let token = presented_authorization(&context).unwrap_or_default();
            return Err(ErrorData::internal_error(
                format!("upstream initialize rejected {INIT_MSG_SENTINEL}: saw bearer {token}"),
                Some(json!({
                    "echo": INIT_DATA_SENTINEL,
                    "presented_bearer": token,
                })),
            ));
        }
        context.peer.set_peer_info(request.clone());
        let mut result = self.negotiate_initialize(&request)?;
        if self.legacy_protocol {
            result.protocol_version = ProtocolVersion::V_2025_06_18;
        }
        Ok(result)
    }

    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        );
        info.server_info = rmcp::model::Implementation::new("cc-upstream", "1.0.0");
        info
    }
}

async fn token_endpoint(
    State(state): State<Arc<UpstreamState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if headers
        .keys()
        .any(|name| name.as_str().eq_ignore_ascii_case(CUSTOM_HEADER))
    {
        state.token_saw_custom_header.store(true, Ordering::SeqCst);
    }
    let form: Vec<(String, String)> = url::form_urlencoded::parse(body.as_ref())
        .into_owned()
        .collect();
    state.token_forms.lock().unwrap().push(form);

    match state.token_mode.lock().unwrap().clone() {
        TokenMode::Issue => {
            let token = state.issue_token();
            let body = json!({
                "access_token": token,
                "token_type": "Bearer",
                "expires_in": state.expires_in,
            });
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body.to_string(),
            )
                .into_response()
        }
        TokenMode::Error { status, code } => (
            StatusCode::from_u16(status).expect("test status"),
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "error": code,
                "error_description": "provider prose that must never reach a client",
            })
            .to_string(),
        )
            .into_response(),
        TokenMode::Redirect => Redirect::temporary("/leak?carrying=client-secret").into_response(),
    }
}

/// The guard's simulated upstream failures, in priority order: redirect to
/// the leak target, session expiry (404 for a session-bound POST) and the
/// 403 challenge. `None` lets the request continue to auth.
fn simulated_upstream_failure(
    state: &Arc<UpstreamState>,
    requests: usize,
    req: &axum::extract::Request,
) -> Option<Response> {
    let threshold = state.redirect_mcp_after.load(Ordering::SeqCst);
    if threshold != usize::MAX && requests > threshold && req.method() == axum::http::Method::POST {
        return Some(Redirect::temporary("/leak?carrying=bearer").into_response());
    }
    // Simulate a server that lost the session: a session-bound POST gets
    // 404, which the rmcp client reads as SessionExpired and answers with an
    // automatic re-initialization.
    if state.expire_sessions.load(Ordering::SeqCst)
        && req.method() == axum::http::Method::POST
        && req.headers().contains_key("mcp-session-id")
    {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    if state.simulate_403.load(Ordering::SeqCst) {
        let challenge = (
            StatusCode::FORBIDDEN,
            [
                (axum::http::header::CONTENT_TYPE, "application/json"),
                (
                    axum::http::header::WWW_AUTHENTICATE,
                    r#"Bearer error="insufficient_scope", error_description="missing scope", scope="admin""#,
                ),
            ],
            json!({"error": "insufficient_scope"}).to_string(),
        );
        return Some(challenge.into_response());
    }
    None
}

/// MCP auth guard: 401 with a text WWW-Authenticate for rejected tokens so the
/// adapter positively identifies the rejection, plus the simulated failure
/// surfaces above.
async fn mcp_guard(
    State(state): State<Arc<UpstreamState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let requests = state.mcp_requests.fetch_add(1, Ordering::SeqCst) + 1;

    if let Some(response) = simulated_upstream_failure(&state, requests, &req) {
        return response;
    }

    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let token = presented
        .as_deref()
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .map(str::to_owned);
    let accepted = token.is_some_and(|token| {
        let valid = state.valid_tokens.lock().unwrap().contains(&token);
        let rejected = state.rejected_tokens.lock().unwrap().contains(&token);
        if valid && !rejected {
            let mut first = state.first_accepted_token.lock().unwrap();
            if first.is_none() {
                *first = Some(token.clone());
            }
            *state.last_authorization.lock().unwrap() = presented.clone();
            true
        } else {
            false
        }
    });
    if accepted {
        return next.run(req).await;
    }
    state.unauthorized_401s.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::UNAUTHORIZED,
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (
                axum::http::header::WWW_AUTHENTICATE,
                r#"Bearer error="invalid_token", error_description="token expired""#,
            ),
        ],
        json!({"error": "invalid_token"}).to_string(),
    )
        .into_response()
}

pub struct ClientCredentialsUpstream {
    pub mcp_url: String,
    pub token_url: String,
    pub state: Arc<UpstreamState>,
    shutdown: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

impl ClientCredentialsUpstream {
    pub async fn spawn() -> Self {
        Self::spawn_bound("127.0.0.1:0").await
    }

    /// Binds on a specific address so tests can prove loopback exceptions:
    /// 127.0.0.2 is a loopback address the proxy's allowlist does not
    /// recognize, standing in for any non-loopback host.
    pub async fn spawn_bound(bind: &str) -> Self {
        Self::spawn_parts(bind, TokenMode::Issue, 3600, false).await
    }

    pub async fn spawn_mode(token_mode: TokenMode, expires_in: u64) -> Self {
        Self::spawn_parts("127.0.0.1:0", token_mode, expires_in, false).await
    }

    /// A stateful upstream: the MCP service issues real session ids, so a
    /// test can expire a session (guard-level 404) and drive the rmcp
    /// client's automatic session recovery, whose failure surfaces through
    /// the B1 outward boundary.
    pub async fn spawn_stateful() -> Self {
        Self::spawn_inner("127.0.0.1:0", TokenMode::Issue, 3600, false, true).await
    }

    pub async fn spawn_parts(
        bind: &str,
        token_mode: TokenMode,
        expires_in: u64,
        include_headers: bool,
    ) -> Self {
        Self::spawn_inner(bind, token_mode, expires_in, include_headers, false).await
    }

    async fn spawn_inner(
        bind: &str,
        token_mode: TokenMode,
        expires_in: u64,
        include_headers: bool,
        stateful: bool,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind(bind).await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let base = format!("http://{}", bind.rsplit_once(':').expect("host:port").0);
        let state = Self::new_state(token_mode, expires_in);
        let app = if stateful {
            build_app_with(
                Arc::clone(&state),
                include_headers,
                LocalSessionManager::default(),
                true,
            )
        } else {
            build_app_with(
                Arc::clone(&state),
                include_headers,
                NeverSessionManager::default(),
                false,
            )
        };
        let shutdown = CancellationToken::new();
        let shutdown_wait = shutdown.clone();
        let join = tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown_wait.cancelled().await;
                })
                .await;
            if let Err(error) = result {
                panic!("client-credentials test upstream failed: {error}");
            }
        });
        Self {
            mcp_url: format!("{base}:{port}/mcp"),
            token_url: format!("{base}:{port}/token"),
            state,
            shutdown,
            join,
        }
    }

    /// Spawns the whole upstream behind TLS that requires a client
    /// certificate signed by the returned CA, so both the token endpoint and
    /// the MCP endpoint are reachable only by clients sharing the configured
    /// TLS identity and CA.
    pub async fn spawn_tls(
        token_mode: TokenMode,
        expires_in: u64,
        include_headers: bool,
    ) -> (Self, TlsClientPaths) {
        let tls = generate_tls_material();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Self::new_state(token_mode, expires_in);
        let app = build_app_with(
            Arc::clone(&state),
            include_headers,
            NeverSessionManager::default(),
            false,
        );
        let shutdown = CancellationToken::new();
        let acceptor = TlsAcceptor::from(Arc::new(tls.server_config));
        let tls_listener = TlsListener { listener, acceptor };
        let shutdown_wait = shutdown.clone();
        let join = tokio::spawn(async move {
            let result = axum::serve(tls_listener, app)
                .with_graceful_shutdown(async move {
                    shutdown_wait.cancelled().await;
                })
                .await;
            if let Err(error) = result {
                panic!("client-credentials TLS upstream failed: {error}");
            }
        });
        (
            Self {
                mcp_url: format!("https://localhost:{port}/mcp"),
                token_url: format!("https://localhost:{port}/token"),
                state,
                shutdown,
                join,
            },
            tls.client_paths,
        )
    }

    fn new_state(token_mode: TokenMode, expires_in: u64) -> Arc<UpstreamState> {
        Arc::new(UpstreamState {
            token_mode: Mutex::new(token_mode),
            expires_in,
            auto_reject_new_tokens: AtomicBool::new(false),
            expire_sessions: AtomicBool::new(false),
            fail_initialize: AtomicBool::new(false),
            valid_tokens: Mutex::new(HashSet::new()),
            rejected_tokens: Mutex::new(HashSet::new()),
            simulate_403: AtomicBool::new(false),
            redirect_mcp_after: AtomicUsize::new(usize::MAX),
            token_exchanges: AtomicUsize::new(0),
            unauthorized_401s: AtomicUsize::new(0),
            mcp_requests: AtomicUsize::new(0),
            leak_hits: AtomicUsize::new(0),
            token_saw_custom_header: AtomicBool::new(false),
            token_forms: Mutex::new(Vec::new()),
            first_accepted_token: Mutex::new(None),
            last_authorization: Mutex::new(None),
            initialize_count: AtomicUsize::new(0),
        })
    }

    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.join.await;
    }
}

/// Writes the secret file, spawns the proxy against `upstream` and returns
/// the rejection message from its eager upstream connect. Panics with `what`
/// when the proxy unexpectedly serves, so a config hole cannot pass silently.
pub async fn rejected_connect_message(
    upstream: &ClientCredentialsUpstream,
    extra: &[&str],
    what: &str,
) -> anyhow::Result<String> {
    let (dir, secret) = write_client_secret_file();
    let args = oauth_args(upstream, &secret, extra);
    let message = match crate::common::spawn_proxy_client_env(&args, &[]).await {
        Err(err) => err.to_string(),
        Ok(_) => anyhow::bail!("{what}: the proxy unexpectedly connected"),
    };
    drop(dir);
    Ok(message)
}

/// Writes the fixture client secret file with native CR/LF semantics covered
/// by the provider itself; returns (tempdir, secret path).
pub fn write_client_secret_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join("client-secret");
    std::fs::write(&path, format!("{CLIENT_SECRET}\n")).expect("write secret");
    (dir, path)
}

/// Proxy argv for client-credentials mode against `upstream`, with extra
/// flags appended verbatim.
pub fn oauth_args(
    upstream: &ClientCredentialsUpstream,
    secret_file: &std::path::Path,
    extra: &[&str],
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--url".into(),
        upstream.mcp_url.clone(),
        "--oauth-token-url".into(),
        upstream.token_url.clone(),
        "--oauth-client-id".into(),
        CLIENT_ID.into(),
        "--oauth-client-secret-file".into(),
        secret_file.display().to_string(),
    ];
    args.extend(extra.iter().map(|arg| arg.to_string()));
    args
}

pub fn echo_args(text: &str) -> rmcp::model::CallToolRequestParams {
    rmcp::model::CallToolRequestParams::new("echo")
        .with_arguments(tool_args(json!({ "text": text })))
}

pub fn call_tool_args(tool: &'static str) -> rmcp::model::CallToolRequestParams {
    rmcp::model::CallToolRequestParams::new(tool).with_arguments(tool_args(json!({ "text": "x" })))
}

/// Waits for `count` to remain unchanged for 200 ms. Fails rather than
/// treating exhaustion of the 500 ms overall deadline as a settled counter.
pub async fn wait_until_settled<T, F>(probe: F) -> anyhow::Result<()>
where
    F: FnMut() -> T,
    T: PartialEq,
{
    wait_until_settled_for(
        probe,
        Duration::from_millis(200),
        Duration::from_millis(500),
        Duration::from_millis(20),
    )
    .await
}

async fn wait_until_settled_for<T, F>(
    mut probe: F,
    quiet_period: Duration,
    overall_timeout: Duration,
    poll_interval: Duration,
) -> anyhow::Result<()>
where
    F: FnMut() -> T,
    T: PartialEq,
{
    let deadline = tokio::time::Instant::now() + overall_timeout;
    let mut last = probe();
    let mut unchanged_since = tokio::time::Instant::now();

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!("counter did not settle before {overall_timeout:?} deadline");
        }
        tokio::time::sleep(poll_interval.min(deadline - now)).await;
        let current = probe();
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!("counter did not settle before {overall_timeout:?} deadline");
        }
        if current != last {
            last = current;
            unchanged_since = now;
        } else if now.duration_since(unchanged_since) >= quiet_period {
            return Ok(());
        }
    }
}

/// Builds the shared upstream app: token endpoint, redirect leak target and
/// the auth-guarded Streamable-HTTP MCP service, over the given session
/// manager (`NeverSessionManager` keeps the stateless default; the stateful
/// tests pass `LocalSessionManager` so session recovery can be driven).
/// `legacy_protocol` negotiates a pre-2026-07-28 version and re-enables
/// `legacy_session_mode`, the only combination that issues real session
/// ids.
fn build_app_with<M: SessionManager>(
    state: Arc<UpstreamState>,
    include_headers: bool,
    session_manager: M,
    legacy_protocol: bool,
) -> Router {
    let handler = CcHandler {
        state: Arc::clone(&state),
        include_headers,
        legacy_protocol,
    };
    let cancellation = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(legacy_protocol)
        .with_json_response(true)
        .with_cancellation_token(cancellation.child_token())
        .disable_allowed_hosts();
    let mcp_service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(session_manager),
        config,
    );
    let mcp_router =
        Router::new()
            .fallback_service(mcp_service)
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                mcp_guard,
            ));
    Router::new()
        .route("/token", post(token_endpoint))
        .route(
            "/leak",
            any(|State(state): State<Arc<UpstreamState>>| async move {
                state.leak_hits.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }),
        )
        .nest_service("/mcp", mcp_router)
        .with_state(state)
}

/// Client TLS material for a test proxy, written to disk as the
/// `--tls-ca`/`--tls-cert`/`--tls-key` flag values.
pub struct TlsClientPaths {
    pub ca: std::path::PathBuf,
    pub cert: std::path::PathBuf,
    pub key: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

struct TlsMaterial {
    server_config: tokio_rustls::rustls::ServerConfig,
    client_paths: TlsClientPaths,
}

fn named_certificate_params(common_name: &str, dns_names: Vec<String>) -> CertificateParams {
    let mut params = CertificateParams::new(dns_names).expect("TLS certificate params");
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params
}

/// Generates a test CA, a server certificate and a client certificate. The
/// server is configured to require a client certificate from that CA, so a
/// proxy can only reach it when both its token client and its MCP client use
/// the same TLS identity and CA.
fn generate_tls_material() -> TlsMaterial {
    // The workspace rustls graph enables both ring and aws-lc-rs, so the
    // test process must pick the provider explicitly (the production binary
    // picks its own inside reqwest).
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring provider");
    // rcgen defaults all three certificates to the same subject. Distinct
    // names keep leaves from looking self-issued to native chain builders.
    let ca_key = KeyPair::generate().expect("generate CA key");
    let mut ca_params = named_certificate_params("harnx mTLS fixture root", vec![]);
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-signed CA");

    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().expect("generate server key");
    let server_params = named_certificate_params("localhost", vec!["localhost".to_string()]);
    let server_cert = server_params
        .signed_by(&server_key, &issuer)
        .expect("server cert");

    let client_key = KeyPair::generate().expect("generate client key");
    let client_params = named_certificate_params("harnx mTLS fixture client", vec![]);
    let client_cert = client_params
        .signed_by(&client_key, &issuer)
        .expect("client cert");

    let mut roots = RootCertStore::empty();
    roots.add(ca_cert.der().clone()).expect("add CA");

    let server_config = tokio_rustls::rustls::ServerConfig::builder()
        .with_client_cert_verifier(
            WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .expect("verifier"),
        )
        .with_single_cert(
            vec![server_cert.der().clone()],
            PrivateKeyDer::try_from(server_key.serialize_der()).expect("server key"),
        )
        .expect("server TLS config");

    let dir = tempfile::TempDir::new().expect("temp dir");
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, ca_cert.pem()).expect("write CA PEM");
    let cert = dir.path().join("client-cert.pem");
    std::fs::write(&cert, client_cert.pem()).expect("write client cert PEM");
    let key = dir.path().join("client-key.pem");
    std::fs::write(&key, client_key.serialize_pem()).expect("write client key PEM");

    TlsMaterial {
        server_config,
        client_paths: TlsClientPaths {
            ca,
            cert,
            key,
            _dir: dir,
        },
    }
}

/// A `tokio_rustls` listener for `axum::serve`. Only the integration test
/// sees handshake details; the proxy's diagnostics stay redacted.
struct TlsListener {
    listener: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls) => return (tls, addr),
                    Err(error) => {
                        let tls = error
                            .get_ref()
                            .and_then(|inner| inner.downcast_ref::<tokio_rustls::rustls::Error>());
                        eprintln!(
                            "fixture accepted TCP from {addr}; TLS failed: io={:?}, rustls={tls:?}",
                            error.kind()
                        );
                    }
                },
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::{generate_tls_material, wait_until_settled, wait_until_settled_for};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn delayed_counter_change_restarts_full_quiet_period() {
        let count = Arc::new(AtomicUsize::new(0));
        let changed = Arc::clone(&count);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            changed.store(1, Ordering::SeqCst);
        });

        let wait = wait_until_settled(|| count.load(Ordering::SeqCst));
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(Duration::from_millis(290), &mut wait)
                .await
                .is_err(),
            "must not return before the full quiet period after the delayed change"
        );
        tokio::time::timeout(Duration::from_millis(50), &mut wait)
            .await
            .expect("counter should settle within its 500 ms deadline")
            .expect("settled helper should succeed");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_exhaustion_is_an_error_not_a_settled_result() {
        let mut count = 0;
        let error = wait_until_settled_for(
            || {
                count += 1;
                count
            },
            Duration::from_millis(50),
            Duration::from_millis(75),
            Duration::from_millis(10),
        )
        .await
        .expect_err("changing counter must exhaust deadline");

        assert!(error.to_string().contains("deadline"), "{error}");
    }

    #[test]
    fn generated_tls_files_are_removed_when_paths_drop() {
        let paths = generate_tls_material().client_paths;
        let files = [paths.ca.clone(), paths.cert.clone(), paths.key.clone()];
        assert!(files.iter().all(|path| path.is_file()));

        drop(paths);

        assert!(
            files.iter().all(|path| !path.exists()),
            "generated TLS files should be deleted with their owning paths"
        );
    }
}
