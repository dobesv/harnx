use super::*;
use harnx_toolset::{ToolInvocation, ToolInvocationContext, ToolProgressKind};

async fn direct_call(server: FsServer, args: Value) -> CallToolResult {
    let connection = connect_server(server, vec![]).await;
    connection
        .client_service
        .peer()
        .call_tool(
            CallToolRequestParams::new("write_bytes")
                .with_arguments(serde_json::from_value(args).unwrap()),
        )
        .await
        .expect("known-tool domain errors must be recoverable")
}

#[tokio::test]
async fn write_bytes_is_hidden_and_not_dispatchable_by_default() {
    assert!(crate::builtin_tool_specs()
        .iter()
        .all(|spec| spec.name != "write_bytes"));
    let root = TestDir::new();
    let path = root.path().join("absent.bin");
    let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]));
    assert!(toolset
        .tools()
        .iter()
        .all(|spec| spec.name != "write_bytes"));
    expect_recoverable(
        invoke_bytes(&toolset, byte_args(&path, "AA==".to_string())).await,
        "unknown tool: write_bytes",
    );
    let connection = connect_server(make_server(root.path()), vec![]).await;
    let peer = connection.client_service.peer();
    let tools = peer.list_tools(None).await.unwrap();
    assert!(tools.tools.iter().all(|tool| tool.name != "write_bytes"));
    let error = peer
        .call_tool(CallToolRequestParams::new("write_bytes"))
        .await
        .expect_err("disabled write_bytes must be unknown, not a domain error");
    assert!(matches!(
        error,
        rmcp::service::ServiceError::McpError(error)
            if error.code == rmcp::model::ErrorCode::INVALID_PARAMS
    ));
    assert!(!path.exists());
}

#[tokio::test]
async fn write_bytes_sandbox_schema_matches_direct_mcp_and_requires_two_strings() {
    let root = TestDir::new();
    let tools = pod_toolset(root.path()).tools();
    let spec = tools
        .iter()
        .find(|spec| spec.name == "write_bytes")
        .unwrap();
    assert_eq!(spec.kind(), Some(ToolProgressKind::Edit));
    assert!(!spec.read_only_hint);
    let connection =
        connect_server(make_server(root.path()).with_sandbox_transport(), vec![]).await;
    let tools = connection
        .client_service
        .peer()
        .list_tools(None)
        .await
        .unwrap();
    let tool = tools
        .tools
        .iter()
        .find(|tool| tool.name == "write_bytes")
        .unwrap();
    let schema = serde_json::to_value(&tool.input_schema).unwrap();
    assert_eq!(schema, spec.input_schema);
    assert_eq!(schema["required"], json!(["path", "data_base64"]));
    assert_eq!(schema["additionalProperties"], false);
    let fields = schema["properties"].as_object().unwrap();
    assert_eq!(fields.len(), 2);
    for field in ["path", "data_base64"] {
        assert_eq!(fields[field]["type"], "string");
    }
    assert_eq!(tool.description.as_deref(), Some(spec.description.as_str()));
    let meta = tool.meta.as_ref().unwrap();
    assert_eq!(
        meta.0["call_template"],
        spec.meta.as_ref().unwrap()["call_template"]
    );
    assert!(meta.0.get("result_template").is_none());
    assert!(spec.meta.as_ref().unwrap().get("result_template").is_none());
    let template = meta.0["call_template"].as_str().unwrap();
    for payload in [
        "",
        "AA==",
        "AAE=",
        "AAEC",
        "invalid base64!",
        &"A".repeat(8192),
    ] {
        let args = json!({"path": "payload.bin", "data_base64": payload});
        let summary = harnx_core::tool::render_tool_call_template(template, &args, "").unwrap();
        assert_eq!(summary, "payload.bin");
    }
}

#[tokio::test]
async fn write_bytes_missing_null_wrong_type_and_unknown_args_are_recoverable() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    for args in [
        json!({}),
        json!({"path": path_string(&path)}),
        json!({"data_base64": "AA=="}),
        json!({"path": null, "data_base64": "AA=="}),
        json!({"path": path_string(&path), "data_base64": null}),
        json!({"path": path_string(&path), "data_base64": 1}),
        json!({"path": 1, "data_base64": "AA=="}),
        json!({"path": path_string(&path), "data_base64": "AA==", "url": "private"}),
    ] {
        expect_recoverable(
            invoke_bytes(&pod_toolset(root.path()), args.clone()).await,
            "invalid tool arguments",
        );
        let result = direct_call(make_server(root.path()).with_sandbox_transport(), args).await;
        assert_eq!(result.is_error, Some(true));
        assert!(text_content(&result).contains("invalid tool arguments"));
        assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    }
}

#[tokio::test]
async fn write_bytes_direct_mcp_decodes_binary_and_maps_bad_base64_to_tool_error() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    let server = make_server(root.path()).with_sandbox_transport();
    let bytes = b"\x89PNG\0\xff";
    let result = direct_call(server.clone(), byte_args(&path, STANDARD.encode(bytes))).await;
    assert_ne!(result.is_error, Some(true));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let result = direct_call(server, byte_args(&path, "bad base64!".to_string())).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).contains("Invalid base64 data:"));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn write_bytes_context_dispatch_keeps_path_observation_without_payload() {
    let root = TestDir::new();
    let path = root.path().join("payload.bin");
    let bytes = b"\x89PNG\0\xff\xfe";
    let encoded = STANDARD.encode(bytes);
    let result = pod_toolset(root.path())
        .invoke_with_context(ToolInvocation {
            tool: "write_bytes".to_string(),
            args: byte_args(&path, encoded.clone()),
            context: ToolInvocationContext::default(),
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let observation = &result["_meta"][EXECUTION_CONTEXT_NAMESPACE];
    let canonical_root = root.path().canonicalize().unwrap();
    assert_eq!(
        observation["working_directory"],
        path_string(&canonical_root)
    );
    assert!(!result.to_string().contains(&encoded));
    assert!(!result.to_string().contains("data_base64"));
}
