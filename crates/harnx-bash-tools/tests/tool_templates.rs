//! The NATS toolset path and the MCP path must render a bash tool the same
//! way. Both read `tool_templates`, so this catches either side drifting.

use harnx_bash_tools::server::BashServer;
use harnx_bash_tools::BashToolset;
use harnx_sandbox_common::SandboxConfig;
use harnx_tool_allow::ResolvedAllowlist;
use harnx_toolset::Toolset;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
use rmcp::service::{serve_client, serve_server};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
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

fn sandbox_config() -> SandboxConfig {
    SandboxConfig {
        enabled: false,
        allowlist: Arc::new(ResolvedAllowlist::new()),
        extra_env_passthrough: vec![],
        env_overrides: vec![],
        sandbox_run_path: PathBuf::from("harnx-sandbox-exec"),
    }
}

/// Call templates advertised over MCP `list_tools`, keyed by tool name.
async fn mcp_call_templates(server: BashServer) -> HashMap<String, String> {
    let (client_transport, server_transport) = duplex(65_536);
    let (server_service, client_service) = tokio::join!(
        serve_server(server, server_transport),
        serve_client(TemplateClientHandler, client_transport)
    );
    let _server_service = server_service.expect("serve bash MCP server");
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
            let template = tool
                .meta
                .as_ref()
                .and_then(|meta| meta.0.get("call_template"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("tool '{}' has no call_template", tool.name));
            (tool.name.to_string(), template.to_string())
        })
        .collect()
}

#[tokio::test]
async fn bash_toolset_call_templates_match_mcp_handler() {
    let advertised = mcp_call_templates(BashServer::new_with_sandbox(sandbox_config())).await;
    assert!(!advertised.is_empty(), "MCP server exposes no tools");

    let toolset = BashToolset::new(sandbox_config()).await;
    let specs = toolset.tools();
    assert_eq!(specs.len(), advertised.len());
    for spec in &specs {
        let meta = spec
            .meta
            .as_ref()
            .unwrap_or_else(|| panic!("tool '{}' spec has no meta", spec.name));
        assert_eq!(
            meta.get("call_template").and_then(Value::as_str),
            advertised.get(&spec.name).map(String::as_str),
            "call_template mismatch for tool '{}'",
            spec.name
        );
        // Bash tools leave `result_template` unset so the client keeps its
        // audience-aware renderer, which surfaces the appended history diff.
        assert!(
            !meta.contains_key("result_template"),
            "tool '{}' must not pin result_template",
            spec.name
        );
    }
    let _ = toolset.cleanup_log_dir();
}
