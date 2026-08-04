use super::*;

#[tokio::test]
async fn fs_tools_advertise_call_template_only() {
    // Each tool ships a `_meta.call_template` for the TUI's call header.
    // We deliberately omit `result_template` so the MCP client falls
    // back to its audience-aware generic renderer — that's what surfaces
    // the history diff content blocks (issue #398).
    let temp_dir = TestDir::new();
    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let tools = peer.list_tools(Default::default()).await.unwrap().tools;
    assert!(!tools.is_empty(), "server should expose at least one tool");
    for tool in &tools {
        let meta = tool
            .meta
            .as_ref()
            .unwrap_or_else(|| panic!("tool '{}' has no _meta", tool.name));
        assert!(
            meta.0.contains_key("call_template"),
            "tool '{}' missing call_template in _meta",
            tool.name
        );
        assert!(
            !meta.0.contains_key("result_template"),
            "tool '{}' must not pin result_template — let the client fall back to its generic audience-aware renderer",
            tool.name
        );
    }
}

#[tokio::test]
async fn test_fs_server_list_tools() {
    let temp_dir = TestDir::new();
    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let tools = peer.list_tools(Default::default()).await.unwrap();
    let names = tools
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "read",
            "write",
            "edit",
            "insert",
            "re_replace",
            "ls",
            "grep",
            "find",
            "rollback_file"
        ]
    );

    // Verify read tool advertises image support in its description
    let read_tool = tools
        .tools
        .iter()
        .find(|t| t.name == "read")
        .expect("read tool should exist");
    let desc = read_tool
        .description
        .as_ref()
        .expect("read tool should have description");
    assert!(
        desc.to_lowercase().contains("image"),
        "read tool description should mention image support"
    );
}

#[tokio::test]
async fn test_fs_server_read_file() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, "notes.txt", "alpha\nbeta\n");

    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let result = peer
        .call_tool(CallToolRequestParams::new("read").with_arguments(tool_args(
            serde_json::json!({
                "path": path_string(&file_path)
            }),
        )))
        .await
        .unwrap();

    let text = text_content(&result);
    assert_eq!(result.is_error, Some(false));
    assert!(text.contains("1: alpha"));
    assert!(text.contains("2: beta"));
}

#[tokio::test]
async fn test_fs_server_write_and_read() {
    let temp_dir = TestDir::new();
    let file_path = fixture_path(&temp_dir, "written.txt");

    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let write_result = peer
        .call_tool(
            CallToolRequestParams::new("write").with_arguments(tool_args(serde_json::json!({
                "path": path_string(&file_path),
                "content": "hello\nworld\n"
            }))),
        )
        .await
        .unwrap();
    assert_eq!(write_result.is_error, Some(false));

    let read_result = peer
        .call_tool(CallToolRequestParams::new("read").with_arguments(tool_args(
            serde_json::json!({
                "path": path_string(&file_path)
            }),
        )))
        .await
        .unwrap();

    let text = text_content(&read_result);
    assert!(text.contains("1: hello"));
    assert!(text.contains("2: world"));
}

#[tokio::test]
async fn test_fs_server_edit_file() {
    let temp_dir = TestDir::new();
    let file_path = write_fixture(&temp_dir, "edit.txt", "old value\n");

    let TestConnection {
        _server_service,
        client_service,
    } = connect_server(
        make_server(temp_dir.path()),
        vec![temp_dir.path().to_path_buf()],
    )
    .await;
    let peer = client_service.peer().clone();
    let _client_task = tokio::spawn(async move {
        let _ = client_service.waiting().await;
    });

    let edit_result = peer
        .call_tool(CallToolRequestParams::new("edit").with_arguments(tool_args(
            serde_json::json!({
                "path": path_string(&file_path),
                "old_text": "old value",
                "new_text": "new value"
            }),
        )))
        .await
        .unwrap();
    assert_eq!(edit_result.is_error, Some(false));
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "new value\n");
}
