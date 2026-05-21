use crate::{start_server, AppState};
use anyhow::Result;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ServerHandler, ServiceExt as _};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::OnceCell;

pub(crate) struct AwsCredsServer {
    state: Arc<AppState>,
    port: OnceCell<u16>,
}

impl ServerHandler for AwsCredsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "harnx-aws-creds",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Provides AWS credentials to Claude Code agents via the AWS Container Credentials protocol.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let input_schema = serde_json::Map::from_iter([
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
        ]);

        Ok(ListToolsResult {
            meta: None,
            tools: vec![Tool::new(
                "aws_auth_setup",
                "Start the AWS credential server and return environment variables to prefix bash commands with for AWS access. Call once per session.",
                input_schema,
            )],
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "aws_auth_setup" => {
                let port = *self
                    .port
                    .get_or_try_init(|| async {
                        let listener = TcpListener::bind("127.0.0.1:0")
                            .await
                            .map_err(|err| ErrorData::internal_error(err.to_string(), None))?;
                        start_server(Arc::clone(&self.state), listener)
                            .await
                            .map_err(|err| ErrorData::internal_error(err.to_string(), None))
                    })
                    .await?;

                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Use these environment variables when running bash commands to access AWS:\n\nAWS_CONTAINER_CREDENTIALS_FULL_URI=http://127.0.0.1:{port}/creds AWS_CONTAINER_AUTHORIZATION_TOKEN={token} AWS_REGION={region}",
                    token = self.state.bearer_token,
                    region = self.state.region,
                ))]))
            }
            name => Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            )),
        }
    }
}

pub(crate) async fn run(state: Arc<AppState>) -> Result<()> {
    let server = AwsCredsServer {
        state,
        port: OnceCell::new(),
    };
    let transport = rmcp::transport::stdio();
    server.serve(transport).await?.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use rmcp::handler::client::ClientHandler;
    use rmcp::model::{CallToolRequestParams, ClientCapabilities, InitializeRequestParams};
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
        _server_service: RunningService<RoleServer, AwsCredsServer>,
        client_service: RunningService<RoleClient, TestClientHandler>,
    }

    async fn connect_server(state: Arc<AppState>) -> TestConnection {
        let (client_transport, server_transport) = duplex(65_536);
        let server = AwsCredsServer {
            state,
            port: OnceCell::new(),
        };
        let server_fut = serve_server(server, server_transport);
        let client_fut = serve_client(TestClientHandler, client_transport);
        let (server_res, client_res) = tokio::join!(server_fut, client_fut);
        TestConnection {
            _server_service: server_res.unwrap(),
            client_service: client_res.unwrap(),
        }
    }

    fn text_content(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .find_map(|content| {
                if let rmcp::model::RawContent::Text(text) = &content.raw {
                    Some(text.text.clone())
                } else {
                    None
                }
            })
            .unwrap()
    }

    fn extract_uri(text: &str) -> Option<String> {
        let prefix = "AWS_CONTAINER_CREDENTIALS_FULL_URI=";
        let start = text.find(prefix)?;
        let rest = &text[start + prefix.len()..];
        let end = rest.find(' ').unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }

    #[tokio::test]
    async fn list_tools_returns_aws_auth_setup() {
        let state = crate::tests::test_state();
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(state).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let tools = peer.list_tools(Default::default()).await.unwrap();
        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.tools[0].name, "aws_auth_setup");
    }

    #[tokio::test]
    async fn call_tool_returns_env_vars() {
        let state = crate::tests::test_state();
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(state).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result = peer
            .call_tool(CallToolRequestParams::new("aws_auth_setup"))
            .await
            .unwrap();

        let text = text_content(&result);
        assert!(text.contains("AWS_CONTAINER_CREDENTIALS_FULL_URI=http://127.0.0.1:"));
        assert!(text.contains("AWS_CONTAINER_AUTHORIZATION_TOKEN=testtoken"));
        assert!(text.contains("AWS_REGION=us-east-1"));
    }

    #[tokio::test]
    async fn call_tool_idempotent() {
        let state = crate::tests::test_state();
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(state).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result1 = peer
            .call_tool(CallToolRequestParams::new("aws_auth_setup"))
            .await
            .unwrap();
        let text1 = text_content(&result1);
        let uri1 = extract_uri(&text1).expect("should have URI in response");

        let result2 = peer
            .call_tool(CallToolRequestParams::new("aws_auth_setup"))
            .await
            .unwrap();
        let text2 = text_content(&result2);
        let uri2 = extract_uri(&text2).expect("should have URI in response");

        assert_eq!(uri1, uri2, "both calls should return same URI port");
    }

    #[tokio::test]
    async fn call_tool_concurrent_calls_both_return_same_port() {
        let state = crate::tests::test_state();
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(state).await;
        let peer = client_service.peer().clone();
        let peer2 = peer.clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let fut1 = peer.call_tool(CallToolRequestParams::new("aws_auth_setup"));
        let fut2 = peer2.call_tool(CallToolRequestParams::new("aws_auth_setup"));
        let (r1, r2) = tokio::join!(fut1, fut2);

        assert!(r1.is_ok(), "first call failed: {r1:?}");
        assert!(r2.is_ok(), "second call failed: {r2:?}");

        let text1 = text_content(&r1.unwrap());
        let text2 = text_content(&r2.unwrap());
        assert_eq!(
            text1, text2,
            "concurrent calls should return identical env vars"
        );
    }

    #[tokio::test]
    async fn call_tool_unknown_returns_error() {
        let state = crate::tests::test_state();
        let TestConnection {
            _server_service,
            client_service,
        } = connect_server(state).await;
        let peer = client_service.peer().clone();
        let _client_task = tokio::spawn(async move {
            let _ = client_service.waiting().await;
        });

        let result = peer
            .call_tool(CallToolRequestParams::new("not_a_tool"))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        // ServiceError::McpError wraps ErrorData which has a `message` field
        match &err {
            rmcp::service::ServiceError::McpError(e) => {
                assert!(e.message.contains("unknown tool"));
            }
            _ => panic!("expected McpError for unknown tool"),
        }
    }
}
