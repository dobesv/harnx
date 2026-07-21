use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::Request;
use axum::middleware::Next;
use rmcp::handler::client::ClientHandler;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ClientCapabilities, ContentBlock, ErrorData, Implementation,
    InitializeRequestParams, ServerInfo,
};
use rmcp::service::{RequestContext, RoleClient, RoleServer};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde_json::{Map, Value, json};
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
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(rmcp::model::ServerCapabilities::builder().enable_tools().build())
    }
}

pub struct HttpTestServerHandle {
    pub port: u16,
    shutdown: CancellationToken,
    join: JoinHandle<()>,
}

impl HttpTestServerHandle {
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.join.await;
    }
}

#[allow(dead_code)]
pub async fn spawn_http_test_server(
    expected_auth: Option<String>,
    include_headers: bool,
) -> Result<HttpTestServerHandle> {
    let shutdown = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_cancellation_token(shutdown.child_token())
        .disable_allowed_hosts();
    let server = TestHttpServer {
        expected_auth,
        include_headers,
        tool_router: TestHttpServer::tool_router(),
    };
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let app = Router::new().nest_service("/mcp", mcp_service);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let shutdown_wait = shutdown.clone();
    let join = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_wait.cancelled().await;
            })
            .await;
        if let Err(err) = result {
            panic!("test HTTP server failed: {err}");
        }
    });
    Ok(HttpTestServerHandle {
        port,
        shutdown,
        join,
    })
}

#[allow(dead_code)]
pub async fn spawn_auth_guard_server(expected_auth: &'static str) -> Result<HttpTestServerHandle> {
    let shutdown = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_cancellation_token(shutdown.child_token())
        .disable_allowed_hosts();
    let server = TestHttpServer {
        expected_auth: None,
        include_headers: false,
        tool_router: TestHttpServer::tool_router(),
    };
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let app = Router::new().nest_service("/mcp", mcp_service).route_layer(
        axum::middleware::from_fn(move |req: Request, next: Next| async move {
            let authorized = req
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                == Some(expected_auth);
            if !authorized {
                return Ok::<_, std::convert::Infallible>(axum::http::Response::builder()
                    .status(axum::http::StatusCode::UNAUTHORIZED)
                    .body(axum::body::Body::from("unauthorized"))
                    .expect("response"));
            }
            Ok::<_, std::convert::Infallible>(next.run(req).await)
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let shutdown_wait = shutdown.clone();
    let join = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_wait.cancelled().await;
            })
            .await;
        if let Err(err) = result {
            panic!("test auth server failed: {err}");
        }
    });
    Ok(HttpTestServerHandle {
        port,
        shutdown,
        join,
    })
}

pub async fn spawn_proxy_client(
    args: &[&str],
) -> Result<(
    rmcp::service::RunningService<RoleClient, TestClientHandler>,
    tokio::process::ChildStderr,
)> {
    let bin = proxy_binary_path().context("harnx-mcp-remote binary not found")?;
    let mut command = tokio::process::Command::new(bin);
    command.args(args);
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn harnx-mcp-remote")?;
    let mut stderr = stderr.expect("proxy stderr should be piped");
    let service = match rmcp::service::serve_client(TestClientHandler, transport).await {
        Ok(service) => service,
        Err(err) => {
            let mut stderr_buf = String::new();
            use tokio::io::AsyncReadExt;
            let _ = stderr.read_to_string(&mut stderr_buf).await;
            return Err(anyhow::anyhow!(
                "connect MCP client to proxy stdio: {err}; proxy stderr: {}",
                stderr_buf.trim()
            ));
        }
    };
    Ok((service, stderr))
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
    let mut exe = std::env::current_exe().expect("current_exe");
    exe.pop();
    if exe.ends_with("deps") {
        exe.pop();
    }
    exe
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
