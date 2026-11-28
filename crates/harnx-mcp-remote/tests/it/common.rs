use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::extract::Request;
use axum::middleware::Next;
use axum::Router;
use rmcp::handler::client::ClientHandler;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ClientCapabilities, ContentBlock, ErrorData, Implementation,
    InitializeRequestParams, InitializeResult, ListPromptsResult, ListResourcesResult,
    PaginatedRequestParams, Prompt, Resource, ServerConfig,
};
use rmcp::service::{
    ClientLifecycleMode, RequestContext, RoleClient, RoleServer, RxJsonRpcMessage, TxJsonRpcMessage,
};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::transport::{TokioChildProcess, Transport};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde_json::{json, Map, Value};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub struct EchoArgs {
    pub text: String,
}

#[derive(Debug)]
pub struct TestClientHandler;

impl ClientHandler for TestClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("harnx-mcp-remote-test-client", "0.1.0"),
        )
    }
}

#[derive(Clone)]
pub struct TestHttpServer {
    expected_auth: Option<String>,
    include_headers: bool,
    initialize_count: Arc<AtomicUsize>,
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

#[tool_router]
impl TestHttpServer {
    #[tool(description = "Echo input text")]
    async fn echo(
        &self,
        Parameters(args): Parameters<EchoArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(expected) = &self.expected_auth {
            let actual = context
                .extensions
                .get::<axum::http::request::Parts>()
                .and_then(|parts| parts.headers.get(axum::http::header::AUTHORIZATION))
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if actual.as_deref() != Some(expected.as_str()) {
                return Err(ErrorData::internal_error(
                    format!("unexpected authorization header: {actual:?}"),
                    None,
                ));
            }
        }

        let mut payload = json!({ "text": args.text });

        if self.include_headers {
            let headers = context
                .extensions
                .get::<axum::http::request::Parts>()
                .map(|parts| header_map_to_json(&parts.headers))
                .unwrap_or_default();
            payload["headers"] = Value::Object(headers);
        }

        Ok(CallToolResult::success(vec![ContentBlock::text(
            payload.to_string(),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for TestHttpServer {
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        self.initialize_count.fetch_add(1, Ordering::SeqCst);
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_completions()
                .build(),
        );
        info.server_info = Implementation::new("upstream-test-server", "1.0.0");
        info.instructions = Some("upstream test instructions".to_owned());
        info.meta = Some(rmcp::model::MetaObject(tool_args(
            json!({"upstream": true}),
        )));
        info
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult {
            prompts: vec![Prompt::new("upstream-prompt", None::<String>, None)],
            ..Default::default()
        })
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![Resource::new("test://upstream", "upstream-resource")],
            ..Default::default()
        })
    }
}

pub struct HttpTestServerHandle {
    pub port: u16,
    pub initialize_count: Arc<AtomicUsize>,
    shutdown: CancellationToken,
    join: JoinHandle<()>,
}

impl HttpTestServerHandle {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub async fn shutdown(self) {
        self.shutdown.cancel();
        self.join.await.expect("test HTTP server task panicked");
    }
}

#[allow(dead_code)]
pub async fn spawn_http_test_server(
    expected_auth: Option<String>,
    include_headers: bool,
) -> Result<HttpTestServerHandle> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    spawn_http_test_server_on_listener(listener, expected_auth, include_headers).await
}

pub async fn spawn_http_test_server_on_listener(
    listener: tokio::net::TcpListener,
    expected_auth: Option<String>,
    include_headers: bool,
) -> Result<HttpTestServerHandle> {
    let shutdown = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(shutdown.child_token())
        .disable_allowed_hosts();
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let server = TestHttpServer {
        expected_auth,
        include_headers,
        initialize_count: initialize_count.clone(),
        tool_router: TestHttpServer::tool_router(),
    };
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let app = Router::new().nest_service("/mcp", mcp_service);
    let (port, join) = serve_test_app(listener, app, shutdown.clone(), "test HTTP server")?;
    Ok(HttpTestServerHandle {
        port,
        initialize_count,
        shutdown,
        join,
    })
}

#[allow(dead_code)]
pub async fn spawn_auth_guard_server(expected_auth: &'static str) -> Result<HttpTestServerHandle> {
    let shutdown = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(shutdown.child_token())
        .disable_allowed_hosts();
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let server = TestHttpServer {
        expected_auth: None,
        include_headers: false,
        initialize_count: initialize_count.clone(),
        tool_router: TestHttpServer::tool_router(),
    };
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let app =
        Router::new()
            .nest_service("/mcp", mcp_service)
            .route_layer(axum::middleware::from_fn(
                move |req: Request, next: Next| async move {
                    let authorized = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        == Some(expected_auth);
                    if !authorized {
                        return Ok::<_, std::convert::Infallible>(
                            axum::http::Response::builder()
                                .status(axum::http::StatusCode::UNAUTHORIZED)
                                .body(axum::body::Body::from("unauthorized"))
                                .expect("response"),
                        );
                    }
                    Ok::<_, std::convert::Infallible>(next.run(req).await)
                },
            ));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let (port, join) = serve_test_app(listener, app, shutdown.clone(), "test auth server")?;
    Ok(HttpTestServerHandle {
        port,
        initialize_count,
        shutdown,
        join,
    })
}

pub async fn spawn_proxy_client(
    args: &[&str],
) -> Result<rmcp::service::RunningService<RoleClient, TestClientHandler>> {
    Ok(
        spawn_proxy_client_with_lifecycle(args, ClientLifecycleMode::Initialize)
            .await?
            .0,
    )
}

pub type RecordedMethods = Arc<Mutex<Vec<String>>>;

struct RecordingTransport<T> {
    inner: T,
    methods: RecordedMethods,
}

impl<T: Transport<RoleClient>> Transport<RoleClient> for RecordingTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let value = serde_json::to_value(&item).expect("serialize outgoing MCP message");
        if let Some(method) = value["method"].as_str() {
            self.methods
                .lock()
                .expect("record methods")
                .push(method.to_owned());
        }
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.inner.receive().await
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}

/// Spawns the proxy binary and connects to it with the given client lifecycle,
/// returning the running service plus a recording of every JSON-RPC method
/// sent downstream.
///
/// Each spawn costs ~3s at teardown: the binary does not exit on stdin EOF
/// (its main waits for a shutdown signal after the transport closes), so
/// rmcp's `TokioChildProcess` shutdown runs its full 3s grace period
/// (`MAX_WAIT_ON_DROP_SECS` in rmcp) before killing the child. Reuse one proxy
/// across assertions where you can; `discover_flow_connects_on_first_proxied_request`
/// spawns four and takes ~12s for this reason.
pub async fn spawn_proxy_client_with_lifecycle(
    args: &[&str],
    lifecycle: ClientLifecycleMode,
) -> Result<(
    rmcp::service::RunningService<RoleClient, TestClientHandler>,
    RecordedMethods,
)> {
    let bin = proxy_binary_path().context("harnx-mcp-remote binary not found")?;
    let mut command = tokio::process::Command::new(bin);
    command.args(args);
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn harnx-mcp-remote")?;
    let mut stderr = stderr.expect("proxy stderr should be piped");
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stderr_buf = String::new();
        let _ = stderr.read_to_string(&mut stderr_buf).await;
        stderr_buf
    });
    let methods = RecordedMethods::default();
    let transport = RecordingTransport {
        inner: transport,
        methods: methods.clone(),
    };
    match rmcp::service::serve_client_with_lifecycle(TestClientHandler, transport, lifecycle).await
    {
        Ok(service) => {
            std::mem::drop(stderr_task);
            Ok((service, methods))
        }
        Err(err) => {
            let stderr_buf = stderr_task
                .await
                .unwrap_or_else(|_| String::from("<stderr task panicked>"));
            Err(anyhow::anyhow!(
                "connect MCP client to proxy stdio: {err}; proxy stderr: {}",
                stderr_buf.trim()
            ))
        }
    }
}

/// Proxy stderr, captured incrementally into a shared buffer so a test can
/// read it while the proxy is still running.
#[derive(Clone)]
pub struct CapturedStderr {
    pub buffer: Arc<Mutex<String>>,
    /// Set by the capture task once the child's stderr closes, so tests can
    /// prove they have drained everything the child ever printed.
    eof: Arc<std::sync::atomic::AtomicBool>,
}

impl CapturedStderr {
    pub fn text(&self) -> String {
        self.buffer.lock().expect("stderr buffer").clone()
    }
}

pub fn capture_stderr(mut stderr: tokio::process::ChildStderr) -> CapturedStderr {
    let captured = CapturedStderr {
        buffer: Arc::new(Mutex::new(String::new())),
        eof: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let output = captured.clone();
    // The capture task owns the pipe and runs until EOF, even if the caller
    // already has a connect error and has not checked stderr yet.
    drop(tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut chunk = [0u8; 4096];
        while let Ok(read) = stderr.read(&mut chunk).await {
            if read == 0 {
                output.eof.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            output
                .buffer
                .lock()
                .expect("stderr buffer")
                .push_str(&String::from_utf8_lossy(&chunk[..read]));
        }
    }));
    captured
}

/// Whether the child's stderr has closed (child exit), so a caller can prove
/// it has drained everything the child ever printed.
pub fn stderr_at_eof(stderr: &CapturedStderr) -> bool {
    stderr.eof.load(std::sync::atomic::Ordering::SeqCst)
}

/// Waits for the child's stderr to reach EOF (child exit), up to `timeout`.
/// Returns `false` on expiry, leaving the caller to assert on the partial
/// capture — a "nothing unsafe was logged" claim is only sound over a
/// drained stderr, so tests fail on `false`.
pub async fn stderr_wait_eof(stderr: &CapturedStderr, timeout: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while !stderr_at_eof(stderr) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    stderr_at_eof(stderr)
}

/// Outcome of spawning the proxy with a live stderr capture, shared by the
/// serving and rejection spawners below.
enum Spawned {
    Served(
        rmcp::service::RunningService<RoleClient, TestClientHandler>,
        RecordedMethods,
        CapturedStderr,
    ),
    /// The downstream initialize was rejected. The transport is already
    /// dropped, so rmcp kills the child (after its grace period) and the
    /// capture reaches EOF on its own.
    Rejected(anyhow::Error, CapturedStderr),
}

/// Spawns the proxy binary with extra child-environment entries and a live,
/// EOF-tracked stderr capture, connecting downstream with the legacy
/// `initialize` lifecycle. The child being rejected at initialize is an
/// `Ok(Spawned::Rejected)`, not a spawn failure.
async fn spawn_proxy_client_captured(args: &[String], env: &[(&str, &str)]) -> Result<Spawned> {
    let bin = proxy_binary_path().context("harnx-mcp-remote binary not found")?;
    let mut command = tokio::process::Command::new(bin);
    command.args(args);
    command.env_remove("MCP_REMOTE_OAUTH_TOKEN_URL");
    command.env_remove("MCP_REMOTE_BEARER_TOKEN");
    command.env_remove("MCP_REMOTE_URL");
    for (name, value) in env {
        command.env(name, value);
    }
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn harnx-mcp-remote")?;
    let captured = capture_stderr(stderr.expect("proxy stderr should be piped"));
    let methods = RecordedMethods::default();
    let transport = RecordingTransport {
        inner: transport,
        methods: methods.clone(),
    };
    let result = rmcp::service::serve_client_with_lifecycle(
        TestClientHandler,
        transport,
        ClientLifecycleMode::Initialize,
    )
    .await;
    match result {
        Ok(service) => Ok(Spawned::Served(service, methods, captured)),
        Err(err) => Ok(Spawned::Rejected(
            anyhow::anyhow!(
                "connect MCP client to proxy stdio: {err}; proxy stderr: {}",
                captured.text()
            ),
            captured,
        )),
    }
}

/// Spawns the proxy binary with extra child-environment entries, connecting
/// downstream with the legacy `initialize` lifecycle and keeping a live
/// stderr capture. Unlike [`spawn_proxy_client_with_lifecycle`], the stderr
/// task is not discarded, so tests can assert on logged output afterwards.
pub async fn spawn_proxy_client_env(
    args: &[String],
    env: &[(&str, &str)],
) -> Result<(
    rmcp::service::RunningService<RoleClient, TestClientHandler>,
    RecordedMethods,
    CapturedStderr,
)> {
    match spawn_proxy_client_captured(args, env).await? {
        Spawned::Served(service, methods, stderr) => Ok((service, methods, stderr)),
        // Rejections surface as the plain connect error, as before; use
        // [`spawn_proxy_client_rejected`] when the drained stderr matters.
        Spawned::Rejected(err, _) => Err(err),
    }
}

/// Spawns the proxy expecting its downstream initialize to be rejected, then
/// drains the child's stderr to EOF and returns the connect error alongside
/// the complete stderr text. Tests that must prove nothing unsafe was logged
/// before or during a rejection need the drained capture, not the prefix
/// captured at the moment the initialize failed.
pub async fn spawn_proxy_client_rejected(
    args: &[String],
    env: &[(&str, &str)],
) -> Result<(String, String)> {
    match spawn_proxy_client_captured(args, env).await? {
        Spawned::Rejected(err, stderr) => {
            // The transport is already dropped; rmcp kills the child after
            // its grace period, which closes stderr.
            if !stderr_wait_eof(&stderr, std::time::Duration::from_secs(10)).await {
                anyhow::bail!("proxy child stderr never reached EOF");
            }
            Ok((err.to_string(), stderr.text()))
        }
        Spawned::Served(service, _, _) => {
            service.cancel().await?;
            anyhow::bail!("expected the proxy to be rejected, but it connected")
        }
    }
}

pub fn text_content(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn tool_args(value: Value) -> Map<String, Value> {
    value.as_object().expect("tool args object").clone()
}

pub fn proxy_binary_path() -> Option<PathBuf> {
    find_binary(
        std::option_env!("CARGO_BIN_EXE_harnx-mcp-remote"),
        "harnx-mcp-remote",
    )
}

fn find_binary(env_path: Option<&str>, name: &str) -> Option<PathBuf> {
    if let Some(path) = env_path {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidate = target_dir().join(binary_name(name));
    candidate.is_file().then_some(candidate)
}

fn target_dir() -> PathBuf {
    harnx_core::bin_dir_of_exe(&std::env::current_exe().expect("current_exe"))
}

fn binary_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn header_map_to_json(headers: &axum::http::HeaderMap) -> Map<String, Value> {
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        let val = value.to_str().unwrap_or_default().to_string();
        grouped.entry(key).or_default().push(val);
    }

    grouped
        .into_iter()
        .map(|(key, values)| {
            let value = if values.len() == 1 {
                Value::String(values.into_iter().next().expect("one value"))
            } else {
                Value::Array(values.into_iter().map(Value::String).collect())
            };
            (key, value)
        })
        .collect()
}

pub fn serve_test_app(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown: CancellationToken,
    name: &'static str,
) -> Result<(u16, tokio::task::JoinHandle<()>)> {
    let port = listener.local_addr()?.port();
    let join = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.cancelled().await;
            })
            .await;
        if let Err(err) = result {
            panic!("{name} failed: {err}");
        }
    });
    Ok((port, join))
}
