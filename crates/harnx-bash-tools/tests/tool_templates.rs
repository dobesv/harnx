//! The NATS toolset path and the MCP path must advertise command templates identically.

use harnx_bash_tools::server::BashServer;
use harnx_bash_tools::{discover_tool_templates, BashToolset};
use harnx_sandbox_common::SandboxConfig;
use harnx_tool_allow::ResolvedAllowlist;
use harnx_toolset::{ToolInvokeError, Toolset};
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, Implementation, InitializeRequestParams,
};
use rmcp::service::{serve_client, serve_server};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::duplex;
use tokio_util::sync::CancellationToken;

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

fn sandbox_config_for(root: &Path) -> SandboxConfig {
    let mut allowlist = ResolvedAllowlist::new();
    allowlist.insert_rwx(root);
    SandboxConfig {
        allowlist: Arc::new(allowlist),
        ..sandbox_config()
    }
}

fn write_template(path: &Path, name: &str, extra: &str, script: &str) {
    std::fs::write(
        path,
        format!(
            "name: {name}\ndescription: Test {name}\n{extra}sandbox:\n  enabled: false\nscript: |\n  {script}\n"
        ),
    )
    .expect("write template");
}

/// Call templates advertised over MCP `list_tools`, keyed by tool name.
async fn mcp_tools(server: BashServer) -> HashMap<String, (String, Value)> {
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
            (
                tool.name.to_string(),
                (
                    template.to_string(),
                    Value::Object((*tool.input_schema).clone()),
                ),
            )
        })
        .collect()
}

async fn mcp_call_template(
    server: BashServer,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let (client_transport, server_transport) = duplex(65_536);
    let (server_service, client_service) = tokio::join!(
        serve_server(server, server_transport),
        serve_client(TemplateClientHandler, client_transport)
    );
    let _server_service = server_service.map_err(|error| error.to_string())?;
    let client_service = client_service.map_err(|error| error.to_string())?;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });
    let arguments = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| "test arguments must be an object".to_string())?;
    let result = peer
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(arguments))
        .await
        .map_err(|error| error.to_string())?;
    serde_json::to_value(result).map_err(|error| error.to_string())
}

#[tokio::test]
async fn bash_toolset_call_templates_match_mcp_handler() {
    let directory = tempfile::tempdir().expect("template directory");
    let template_path = directory.path().join("echo_num.yaml");
    write_template(
        &template_path,
        "echo_num",
        "parameters:\n  number: { type: integer, required: true }\n",
        "echo \"$NUMBER\"",
    );
    let templates =
        discover_tool_templates(None, &[template_path], &[]).expect("discover command template");
    let server = BashServer::new_with_templates(sandbox_config(), templates.clone())
        .expect("build MCP server");
    let advertised = mcp_tools(server).await;
    assert!(advertised.contains_key("echo_num"));

    let toolset = BashToolset::new(sandbox_config(), templates)
        .await
        .expect("build toolset");
    let specs = toolset.tools();
    assert_eq!(specs.len(), advertised.len());
    for spec in &specs {
        let meta = spec
            .meta
            .as_ref()
            .unwrap_or_else(|| panic!("tool '{}' spec has no meta", spec.name));
        let (call_template, input_schema) = advertised
            .get(&spec.name)
            .unwrap_or_else(|| panic!("tool '{}' missing from MCP handler", spec.name));
        assert_eq!(
            meta.get("call_template").and_then(Value::as_str),
            Some(call_template.as_str()),
            "call_template mismatch for tool '{}'",
            spec.name
        );
        assert_eq!(
            &spec.input_schema, input_schema,
            "input schema mismatch for tool '{}'",
            spec.name
        );
        assert!(
            !meta.contains_key("result_template"),
            "tool '{}' must not pin result_template",
            spec.name
        );
    }
    let _ = toolset.cleanup_log_dir();
}

#[tokio::test]
async fn direct_mcp_handler_invokes_template() {
    let directory = tempfile::tempdir().expect("template directory");
    let template_path = directory.path().join("echo_num.yaml");
    write_template(
        &template_path,
        "echo_num",
        "parameters:\n  number: { type: integer, required: true }\n",
        "echo \"$NUMBER\"",
    );
    let templates =
        discover_tool_templates(None, &[template_path], &[]).expect("discover command template");
    let server = BashServer::new_with_templates(sandbox_config_for(directory.path()), templates)
        .expect("build MCP server");

    let result = mcp_call_template(server, "echo_num", json!({ "number": 9 }))
        .await
        .expect("call template over direct MCP handler");
    assert!(
        result.to_string().contains("9"),
        "unexpected MCP result: {result}"
    );
}

#[tokio::test]
async fn template_tool_validates_then_runs_through_exec_pipeline() {
    let directory = tempfile::tempdir().expect("template directory");
    let marker = directory.path().join("ran.marker");
    let template_path = directory.path().join("echo_num.yaml");
    write_template(
        &template_path,
        "echo_num",
        &format!(
            "parameters:\n  number: {{ type: integer, required: true }}\nenv:\n  MARKER: {}\n",
            serde_json::to_string(&marker.to_string_lossy()).expect("quote marker path")
        ),
        "echo \"$NUMBER\"; touch \"$MARKER\"",
    );
    let templates =
        discover_tool_templates(None, &[template_path], &[]).expect("discover command template");
    let toolset = BashToolset::new(sandbox_config_for(directory.path()), templates)
        .await
        .expect("build toolset");

    let echo = toolset
        .tools()
        .into_iter()
        .find(|tool| tool.name == "echo_num")
        .expect("echo_num is registered");
    assert_eq!(
        echo.input_schema["properties"]["number"]["type"],
        json!("integer")
    );

    let result = toolset
        .invoke("echo_num", json!({ "number": 7 }), CancellationToken::new())
        .await
        .expect("valid template call");
    assert!(
        result.to_string().contains("7"),
        "unexpected result: {result}"
    );
    assert!(marker.exists(), "valid invocation did not execute script");

    std::fs::remove_file(&marker).expect("remove marker");
    let error = toolset
        .invoke(
            "echo_num",
            json!({ "number": "x" }),
            CancellationToken::new(),
        )
        .await
        .expect_err("invalid template call must fail");
    assert!(matches!(error, ToolInvokeError::Recoverable(_)));
    assert!(
        error.to_string().contains("integer"),
        "unexpected error: {error}"
    );
    assert!(!marker.exists(), "validation failure executed script");
    let _ = toolset.cleanup_log_dir();
}

#[tokio::test]
async fn reserved_builtin_name_is_hard_error_when_explicit_and_skipped_when_auto() {
    let package = tempfile::tempdir().expect("package directory");
    let auto_dir = package.path().join("bash_tools");
    std::fs::create_dir(&auto_dir).expect("create bash_tools");
    let template_path = auto_dir.join("exec.yaml");
    write_template(&template_path, "exec", "", "echo shadowed");

    let explicit_error = discover_tool_templates(None, std::slice::from_ref(&template_path), &[])
        .expect_err("explicit built-in collision must fail");
    assert!(
        explicit_error
            .to_string()
            .contains("reserved built-in tool name `exec`"),
        "unexpected error: {explicit_error:#}"
    );

    let templates = discover_tool_templates(Some(package.path()), &[], &[])
        .expect("auto collision should be skipped");
    assert!(templates.is_empty());
    let toolset = BashToolset::new(sandbox_config_for(package.path()), templates)
        .await
        .expect("build toolset after skipped collision");
    assert_eq!(
        toolset
            .tools()
            .iter()
            .filter(|tool| tool.name == "exec")
            .count(),
        1
    );
    let result = toolset
        .invoke(
            "exec",
            json!({
                "command": "printf builtin-exec",
                "working_dir": package.path().to_string_lossy()
            }),
            CancellationToken::new(),
        )
        .await
        .expect("built-in exec remains callable");
    assert!(result.to_string().contains("builtin-exec"));
    let _ = toolset.cleanup_log_dir();
}
