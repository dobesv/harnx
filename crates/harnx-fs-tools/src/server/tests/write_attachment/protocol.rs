use super::*;
use harnx_toolset::{
    ToolInvocation, ToolInvocationContext, ToolProgress, ToolProgressHandle, ToolProgressKind,
    ToolProgressPatch,
};

async fn mcp_write(server: FsServer, args: Value) -> CallToolResult {
    let connection = connect_server(server, vec![]).await;
    connection
        .client_service
        .peer()
        .call_tool(CallToolRequestParams::new("write_attachment").with_arguments(tool_args(args)))
        .await
        .expect("domain failures must return tool results, not MCP errors")
}

#[tokio::test]
async fn direct_mcp_write_attachment_preserves_binary_empty_and_plan_bytes() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "long original content");
    let media = media_url();
    for (url, bytes) in [
        (media.as_str(), b"\x89PNG\r\n\x1a\n\xff\0".as_slice()),
        (media.as_str(), b"".as_slice()),
        (PLAN_URL, "# Plan\n\nExact café Markdown\n".as_bytes()),
    ] {
        let resolver = FakeResolver::new(url, Ok(bytes.to_vec()));
        let result = mcp_write(
            make_server(root.path()).with_attachment_resolver(resolver.clone()),
            attachment_args(&path, url),
        )
        .await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn direct_mcp_write_attachment_resolution_failures_are_tool_errors() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    for (error, expected) in resolution_failures() {
        let result = mcp_write(
            make_server(root.path())
                .with_attachment_resolver(FakeResolver::new(PLAN_URL, Err(error))),
            attachment_args(&path, PLAN_URL),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        assert!(text_content(&result).contains(&expected), "{result:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    }
}

#[tokio::test]
async fn direct_mcp_write_attachment_validation_and_unavailable_errors_are_tool_errors() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    for url in ["", " ", "cid:invalid:value", PLAN_URL] {
        let result = mcp_write(make_server(root.path()), attachment_args(&path, url)).await;
        assert_eq!(result.is_error, Some(true));
        let expected = if url == PLAN_URL {
            "Attachment resolution unavailable: no resolver configured for this filesystem server."
        } else {
            "Invalid attachment URL:"
        };
        assert!(text_content(&result).contains(expected), "{result:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    }
    let result = mcp_write(
        FsServer::new(ResolvedAllowlist::new()),
        attachment_args(&path, PLAN_URL),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text_content(&result).to_lowercase().contains("denied"));
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
}

#[tokio::test]
async fn write_attachment_requires_non_null_path_and_url_on_both_dispatch_paths() {
    let root = TestDir::new();
    let path = write_fixture(&root, "existing.bin", "unchanged");
    let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]));
    for args in [
        json!({}),
        json!({"path": path_string(&path)}),
        json!({"url": PLAN_URL}),
        json!({"path": null, "url": PLAN_URL}),
        json!({"path": path_string(&path), "url": null}),
        json!({"path": path_string(&path), "url": 1}),
        json!({"path": 1, "url": PLAN_URL}),
        json!({"path": path_string(&path), "url": PLAN_URL, "data_base64": "AA=="}),
    ] {
        let native = toolset
            .invoke("write_attachment", args.clone(), CancellationToken::new())
            .await;
        assert!(recoverable_message(native).contains("invalid tool arguments"));
        let mcp = mcp_write(make_server(root.path()), args).await;
        assert_eq!(mcp.is_error, Some(true));
        assert!(text_content(&mcp).contains("invalid tool arguments"));
        assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    }
}

fn assert_required_strings(schema: &Value, fields: &[&str]) {
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], json!(fields));
    assert_eq!(schema["additionalProperties"], false);
    let properties = schema["properties"].as_object().unwrap();
    assert_eq!(properties.len(), fields.len());
    for field in fields {
        assert_eq!(properties[*field]["type"], "string");
    }
}

#[tokio::test]
async fn write_attachment_schema_description_and_template_match_direct_mcp() {
    let root = TestDir::new();
    let connection = connect_server(make_server(root.path()), vec![]).await;
    let tools = connection
        .client_service
        .peer()
        .list_tools(Default::default())
        .await
        .unwrap()
        .tools;
    let specs = crate::builtin_tool_specs();
    let spec = specs
        .iter()
        .find(|tool| tool.name == "write_attachment")
        .unwrap();
    let mcp = tools
        .iter()
        .find(|tool| tool.name == "write_attachment")
        .unwrap();
    let schema = mcp.schema_as_json_value();
    assert_eq!(spec.input_schema, schema);
    assert_required_strings(&schema, &["path", "url"]);
    assert_eq!(spec.description, "Copy exact bytes from a cid:media: attachment, or rendered Markdown from a cid:plan: URL, to a file. Creates or overwrites destination. No text decoding.");
    assert_eq!(mcp.description.as_deref(), Some(spec.description.as_str()));
    assert_eq!(spec.kind(), Some(ToolProgressKind::Edit));
    assert!(!spec.read_only_hint);
    let meta = spec.meta.as_ref().unwrap();
    assert_eq!(meta["call_template"], "{{ args.path }} ← {{ args.url }}");
    assert!(!meta.contains_key("result_template"));
    assert_eq!(
        mcp.meta.as_ref().unwrap().0["call_template"],
        meta["call_template"]
    );
    let write = specs.iter().find(|tool| tool.name == "write").unwrap();
    assert_required_strings(&write.input_schema, &["path", "content"]);
    assert!(write
        .description
        .ends_with("To copy attachment bytes (including binary), use write_attachment."));
    let mcp_write = tools.iter().find(|tool| tool.name == "write").unwrap();
    assert_eq!(write.input_schema, mcp_write.schema_as_json_value());
    assert_eq!(
        mcp_write.description.as_deref(),
        Some(write.description.as_str())
    );
}

#[derive(Default)]
struct RecordingProgress(Mutex<Vec<ToolProgressPatch>>);

impl ToolProgress for RecordingProgress {
    fn update(&self, patch: ToolProgressPatch) {
        self.0.lock().unwrap().push(patch);
    }
}

#[tokio::test]
async fn write_attachment_emits_edit_progress_and_destination_observation() {
    let root = TestDir::new();
    let path = root
        .path()
        .canonicalize()
        .unwrap()
        .join("nested/target.bin");
    let resolver = FakeResolver::new(PLAN_URL, Ok(vec![255, 0]));
    let toolset = FsToolset::new(rwx_allowlist([root.path().to_path_buf()]))
        .with_attachment_resolver(resolver);
    let progress = Arc::new(RecordingProgress::default());
    let result = toolset
        .invoke_with_context(ToolInvocation {
            tool: "write_attachment".to_string(),
            args: attachment_args(&path, PLAN_URL),
            context: ToolInvocationContext {
                progress: ToolProgressHandle::new(progress.clone()),
                ..Default::default()
            },
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    assert_eq!(result["isError"], false);
    let patches = progress.0.lock().unwrap();
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].kind, Some(ToolProgressKind::Edit));
    assert_eq!(patches[0].locations.as_ref().unwrap()[0].path, path);
    assert!(patches[0]
        .title
        .as_ref()
        .unwrap()
        .starts_with("Writing attachment to "));
    assert_eq!(
        result["_meta"][EXECUTION_CONTEXT_NAMESPACE]["working_directory"],
        path_string(path.parent().unwrap())
    );
    assert_eq!(std::fs::read(path).unwrap(), [255, 0]);
}
