//! The NATS toolset path and the MCP path must render a plans tool the same
//! way. Both read `tool_templates`, so this catches either side drifting.

use harnx_plans_tools::server::PlansServer;
use harnx_plans_tools::PlansToolset;
use harnx_toolset::Toolset;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
use rmcp::service::{serve_client, serve_server};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::duplex;

#[derive(Clone, Default)]
struct TemplateClientHandler;

impl ClientHandler for TemplateClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("test", "0.1"),
        )
    }
}

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("harnx-plans-tool-templates-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp plans dir");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Templates advertised over MCP `list_tools`, keyed by tool name.
async fn mcp_templates(server: PlansServer) -> HashMap<String, (String, String)> {
    let (client_transport, server_transport) = duplex(65_536);
    let (server_service, client_service) = tokio::join!(
        serve_server(server, server_transport),
        serve_client(TemplateClientHandler, client_transport)
    );
    let _server_service = server_service.expect("serve plans MCP server");
    let client_service = client_service.expect("serve MCP client");
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    peer.list_tools(Default::default())
        .await
        .expect("list tools")
        .tools
        .iter()
        .map(|tool| {
            let meta = tool
                .meta
                .as_ref()
                .unwrap_or_else(|| panic!("tool '{}' has no _meta", tool.name));
            let template = |key: &str| {
                meta.0
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("tool '{}' has no {key}", tool.name))
                    .to_string()
            };
            (
                tool.name.to_string(),
                (template("call_template"), template("result_template")),
            )
        })
        .collect()
}

#[tokio::test]
async fn plans_toolset_templates_match_mcp_handler() {
    let dir = TestDir::new();
    let advertised = mcp_templates(PlansServer::new(dir.0.clone())).await;
    assert!(!advertised.is_empty(), "MCP server exposes no tools");

    let specs = PlansToolset::new(dir.0.clone()).tools();
    assert_eq!(specs.len(), advertised.len());
    for spec in &specs {
        let meta = spec
            .meta
            .as_ref()
            .unwrap_or_else(|| panic!("tool '{}' spec has no meta", spec.name));
        let (call_template, result_template) = advertised
            .get(&spec.name)
            .unwrap_or_else(|| panic!("MCP handler does not advertise tool '{}'", spec.name));
        assert_eq!(
            meta.get("call_template").and_then(Value::as_str),
            Some(call_template.as_str()),
            "call_template mismatch for tool '{}'",
            spec.name
        );
        assert_eq!(
            meta.get("result_template").and_then(Value::as_str),
            Some(result_template.as_str()),
            "result_template mismatch for tool '{}'",
            spec.name
        );
    }
}
