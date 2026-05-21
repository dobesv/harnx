use crate::{ca::CaSetup, filter::CompiledFilter, proxy};
use anyhow::Result;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ServerHandler, ServiceExt as _};
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::sync::{Mutex, OnceCell};

struct PreStartState {
    filter: CompiledFilter,
    ca_setup: CaSetup,
}

pub(crate) struct ProxyAuthConfig {
    pub filter: CompiledFilter,
    pub ca_setup: CaSetup,
    pub ca_cert_path: PathBuf,
    pub ca_temp_dir: TempDir,
    pub services: Option<String>,
}

pub(crate) struct ProxyAuthServer {
    pre_start: Mutex<Option<PreStartState>>,
    ca_cert_path: PathBuf,
    _ca_temp_dir: TempDir,
    services: Option<String>,
    started: OnceCell<u16>,
}

impl ProxyAuthServer {
    fn tool_description(&self) -> String {
        match self.services.as_deref() {
            Some(services) => format!(
                "Start auth proxy and return environment variables for bash commands to access {services}."
            ),
            None => "Start auth proxy and return environment variables for bash commands to route through MITM proxy."
                .to_string(),
        }
    }

    async fn proxy_port(&self) -> Result<u16, ErrorData> {
        // `OnceCell::get_or_try_init` ensures only one caller runs the closure,
        // even under concurrent access — others wait. On success the port is
        // cached and returned on every subsequent call without re-locking.
        //
        // Failure handling: `CaSetup` is consumed by `start_proxy`, so we
        // cannot retry after a failure. If startup fails the closure returns
        // `Err`, `OnceCell` stays uninit, and the next call sees `None` in
        // `pre_start` — we return a clear error asking the user to restart.
        // This is intentional: a startup failure indicates a misconfiguration
        // or system problem that requires a process restart to fix.
        let port = self
            .started
            .get_or_try_init(|| async {
                let mut guard = self.pre_start.lock().await;
                match guard.take() {
                    Some(pre_start) => {
                        // Drop the lock before the async call so we don't hold
                        // a mutex across an await point.
                        drop(guard);
                        proxy::start_proxy(pre_start.filter, pre_start.ca_setup)
                            .await
                            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
                    }
                    None => {
                        // Startup was previously attempted but failed (CaSetup
                        // was consumed). Restart the process to try again.
                        Err(ErrorData::internal_error(
                            "proxy startup previously failed; restart the server to retry",
                            None,
                        ))
                    }
                }
            })
            .await?;
        Ok(*port)
    }

    async fn call_proxy_auth_setup(&self) -> Result<CallToolResult, ErrorData> {
        let port = self.proxy_port().await?;
        let ca_cert_path = self.ca_cert_path.display();
        let proxy_env = format!(
            "HTTP_PROXY=http://127.0.0.1:{port}\nHTTPS_PROXY=http://127.0.0.1:{port}\nSSL_CERT_FILE={ca_cert_path}\nREQUESTS_CA_BUNDLE={ca_cert_path}\nNODE_EXTRA_CA_CERTS={ca_cert_path}"
        );
        let text = match self.services.as_deref() {
            Some(services) => format!(
                "Use these environment variables when running bash commands to access {services}:\n\n{proxy_env}"
            ),
            None => format!(
                "Use these environment variables when running bash commands through auth proxy:\n\n{proxy_env}"
            ),
        };
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

impl ServerHandler for ProxyAuthServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-proxy-auth",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Provides a local auth proxy to Claude Code agents via an MCP tool.".to_string(),
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: vec![Tool::new(
                "proxy_auth_setup",
                self.tool_description(),
                serde_json::Map::from_iter([
                    (
                        "type".to_string(),
                        serde_json::Value::String("object".to_string()),
                    ),
                    (
                        "properties".to_string(),
                        serde_json::Value::Object(serde_json::Map::new()),
                    ),
                    (
                        "additionalProperties".to_string(),
                        serde_json::Value::Bool(false),
                    ),
                ]),
            )],
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "proxy_auth_setup" => self.call_proxy_auth_setup().await,
            name => Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            )),
        }
    }
}

pub(crate) async fn run(config: ProxyAuthConfig) -> Result<()> {
    let server = ProxyAuthServer {
        pre_start: Mutex::new(Some(PreStartState {
            filter: config.filter,
            ca_setup: config.ca_setup,
        })),
        ca_cert_path: config.ca_cert_path,
        _ca_temp_dir: config.ca_temp_dir,
        services: config.services,
        started: OnceCell::new(),
    };
    let transport = rmcp::transport::stdio();
    server.serve(transport).await?.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ca, filter};
    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{ClientCapabilities, InitializeRequestParams, RawContent};
    use rmcp::service::{serve_client, serve_server, RoleClient, RoleServer, RunningService};
    use tokio::io::duplex;

    #[derive(Clone, Default)]
    struct TestClientHandler;

    impl ClientHandler for TestClientHandler {
        fn get_info(&self) -> InitializeRequestParams {
            InitializeRequestParams::new(
                ClientCapabilities::builder().build(),
                Implementation::new("test", "0.1"),
            )
        }
    }

    struct TestConnection {
        _server_service: RunningService<RoleServer, ProxyAuthServer>,
        client_service: RunningService<RoleClient, TestClientHandler>,
    }

    fn make_server(services: Option<String>) -> ProxyAuthServer {
        let (ca_setup, ca_temp_dir) = ca::setup().expect("ca setup should succeed");
        let ca_cert_path = ca_setup.cert_pem_path.clone();
        let filter = filter::compile(".").expect("filter compile should succeed");
        ProxyAuthServer {
            pre_start: Mutex::new(Some(PreStartState { filter, ca_setup })),
            ca_cert_path,
            _ca_temp_dir: ca_temp_dir,
            services,
            started: OnceCell::new(),
        }
    }

    async fn connect_server(server: ProxyAuthServer) -> TestConnection {
        let (client_transport, server_transport) = duplex(65_536);
        let server_fut = serve_server(server, server_transport);
        let client_fut = serve_client(TestClientHandler, client_transport);
        let (server_res, client_res) = tokio::join!(server_fut, client_fut);
        TestConnection {
            _server_service: server_res.unwrap(),
            client_service: client_res.unwrap(),
        }
    }

    async fn list_tools_for(services: Option<String>) -> ListToolsResult {
        let server = make_server(services);
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(server).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        peer.list_tools(Default::default()).await.unwrap()
    }

    fn text_content(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .find_map(|content| {
                if let RawContent::Text(text) = &content.raw {
                    Some(text.text.clone())
                } else {
                    None
                }
            })
            .expect("response should contain text content")
    }

    fn extract_port(text: &str) -> Option<String> {
        let prefix = "HTTP_PROXY=http://127.0.0.1:";
        let start = text.find(prefix)?;
        let rest = &text[start + prefix.len()..];
        let end = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        Some(rest[..end].to_string())
    }

    #[tokio::test]
    async fn list_tools_without_services() {
        let result = list_tools_for(None).await;
        let tool = &result.tools[0];
        let description = tool.description.as_deref().unwrap();
        assert!(description.contains("auth proxy"));
        assert!(!description.contains("GitHub"));
        assert!(!description.contains("Jira"));
    }

    #[tokio::test]
    async fn list_tools_with_services() {
        let result = list_tools_for(Some("GitHub, Jira".to_string())).await;
        let tool = &result.tools[0];
        assert!(tool
            .description
            .as_deref()
            .unwrap()
            .contains("GitHub, Jira"));
    }

    #[tokio::test]
    async fn call_tool_returns_proxy_env_vars() {
        let server = make_server(None);
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(server).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result = peer
            .call_tool(CallToolRequestParams::new("proxy_auth_setup"))
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("HTTP_PROXY=http://127.0.0.1:"));
        assert!(text.contains("HTTPS_PROXY=http://127.0.0.1:"));
        assert!(text.contains("SSL_CERT_FILE="));
        assert!(text.contains("REQUESTS_CA_BUNDLE="));
        assert!(text.contains("NODE_EXTRA_CA_CERTS="));
    }

    #[tokio::test]
    async fn call_tool_with_services_mentions_services_in_response() {
        let server = make_server(Some("Jira".to_string()));
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(server).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result = peer
            .call_tool(CallToolRequestParams::new("proxy_auth_setup"))
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("Jira"));
    }

    #[tokio::test]
    async fn call_tool_idempotent() {
        let server = make_server(None);
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(server).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result1 = peer
            .call_tool(CallToolRequestParams::new("proxy_auth_setup"))
            .await
            .unwrap();
        let text1 = text_content(&result1);
        let port1 = extract_port(&text1).expect("should have port in response");

        let result2 = peer
            .call_tool(CallToolRequestParams::new("proxy_auth_setup"))
            .await
            .unwrap();
        let text2 = text_content(&result2);
        let port2 = extract_port(&text2).expect("should have port in response");

        assert_eq!(port1, port2, "both calls should return same port");
    }

    #[tokio::test]
    async fn call_tool_concurrent_calls_both_succeed() {
        let server = make_server(None);
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(server).await;
        let peer = client_service.peer().clone();
        let peer2 = peer.clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let fut1 = peer.call_tool(CallToolRequestParams::new("proxy_auth_setup"));
        let fut2 = peer2.call_tool(CallToolRequestParams::new("proxy_auth_setup"));
        let (r1, r2) = tokio::join!(fut1, fut2);

        assert!(r1.is_ok(), "first call failed: {r1:?}");
        assert!(r2.is_ok(), "second call failed: {r2:?}");

        let text1 = text_content(&r1.unwrap());
        let text2 = text_content(&r2.unwrap());
        assert_eq!(extract_port(&text1), extract_port(&text2));
    }

    #[tokio::test]
    async fn call_tool_unknown_returns_error() {
        let server = make_server(None);
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(server).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result = peer
            .call_tool(CallToolRequestParams::new("not_a_tool"))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            rmcp::service::ServiceError::McpError(e) => {
                assert!(e.message.contains("unknown tool"));
            }
            _ => panic!("expected McpError for unknown tool"),
        }
    }

    #[tokio::test]
    async fn call_tool_after_startup_failure_returns_descriptive_error() {
        // Simulate a server whose pre_start state was consumed by a previous
        // failed startup attempt (pre_start = None, started = uninit).
        // The tool call should return a descriptive error, not a misleading
        // "state missing" message.
        let (_, ca_temp_dir) = ca::setup().expect("ca setup");
        let server = ProxyAuthServer {
            pre_start: Mutex::new(None), // simulates post-failure state
            ca_cert_path: ca_temp_dir.path().join("ca.pem"),
            _ca_temp_dir: ca_temp_dir,
            services: None,
            started: OnceCell::new(),
        };
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(server).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result = peer
            .call_tool(CallToolRequestParams::new("proxy_auth_setup"))
            .await;

        assert!(result.is_err(), "should fail when pre_start is None");
        let err = result.unwrap_err();
        match &err {
            rmcp::service::ServiceError::McpError(e) => {
                assert!(
                    e.message.contains("restart"),
                    "error should mention restart: {}",
                    e.message
                );
            }
            _ => panic!("expected McpError: {err:?}"),
        }
    }
}
