#![cfg(unix)]
use std::process::Stdio;

use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, Implementation, InitializeRequestParams,
};
use rmcp::service::RoleClient;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::process::Command;

struct RawClientHandler;

impl ClientHandler for RawClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new("raw-rmcp-test-client", env!("CARGO_PKG_VERSION")),
        )
    }
}

#[tokio::test]
async fn raw_rmcp_client_drives_plans_server_over_stdio() {
    let plans_dir = tempfile::tempdir().expect("create temporary plans directory");
    let mut child = Command::new(env!("CARGO_BIN_EXE_harnx-plans-tools"))
        .arg("--mcp-stdio")
        .arg("--dir")
        .arg(plans_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn harnx-plans-tools");

    let child_stdin = child.stdin.take().expect("take child stdin");
    let child_stdout = child.stdout.take().expect("take child stdout");
    let transport = AsyncRwTransport::<RoleClient, _, _>::new(child_stdout, child_stdin);
    let service = rmcp::service::serve_client(RawClientHandler, transport)
        .await
        .expect("initialize raw rmcp client");
    let peer = service.peer().clone();

    let tools = peer
        .list_tools(Default::default())
        .await
        .expect("list plans tools");
    assert_eq!(tools.tools.len(), 15);
    assert!(tools.tools.iter().all(|tool| !tool.input_schema.is_empty()));

    let tool_names = tools
        .tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<Vec<_>>();
    for expected in ["list_plans", "add_plan", "add_note"] {
        assert!(tool_names.contains(&expected), "missing tool {expected}");
    }

    let result = peer
        .call_tool(CallToolRequestParams::new("list_plans"))
        .await
        .expect("call list_plans");
    assert_ne!(result.is_error, Some(true));
    assert!(!result.content.is_empty());

    service.cancel().await.expect("close raw rmcp client");
    child.kill().await.expect("stop harnx-plans-tools");
}
