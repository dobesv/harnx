use super::*;

async fn tools_with_defaults(root: &Path, script: &str, output: &str) -> (BashServer, BashToolset) {
    let path = root.join("defaults.yaml");
    write_template(&path, "output", &format!("output: {output}\n"), script);
    let templates = discover_tool_templates(None, &[path], &[]).unwrap();
    let server =
        BashServer::new_with_templates(sandbox_config_for(root), templates.clone()).unwrap();
    let toolset = BashToolset::new(sandbox_config_for(root), templates)
        .await
        .unwrap();
    (server, toolset)
}

async fn invoke_both_paths(server: &BashServer, toolset: &BashToolset, args: Value) -> [Value; 2] {
    [
        toolset
            .invoke("output", args.clone(), CancellationToken::new())
            .await
            .unwrap(),
        mcp_call_template(server.clone(), "output", args)
            .await
            .unwrap(),
    ]
}

#[tokio::test]
async fn template_default_byte_limit_recovers_long_lines_on_native_and_mcp_paths() {
    let dir = tempfile::tempdir().unwrap();
    let (server, toolset) =
        tools_with_defaults(dir.path(), &long_lines(5000), "{max_output_bytes: 65536}").await;
    for result in invoke_both_paths(&server, &toolset, json!({})).await {
        assert_template_long_lines(&result, 5000, true);
    }
    for result in invoke_both_paths(&server, &toolset, json!({"max_output_bytes": 1024})).await {
        assert_ne!(result["isError"], true, "{result}");
        let text = assistant_text(&result);
        assert!(!text.contains(&"0".repeat(5000)), "{text}");
        assert!(!text.contains("removed from line"), "{text}");
        assert!(text.contains("stdout truncated"), "{text}");
    }
    toolset.cleanup_log_dir().await.unwrap();
}

#[tokio::test]
async fn template_default_line_windows_allow_independent_caller_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let (server, toolset) = tools_with_defaults(
        dir.path(),
        "printf 'first\\nmiddle\\nlast\\n'",
        "{head_lines: 1, tail_lines: 0, max_output_bytes: 65536}",
    )
    .await;
    for (args, first, middle, last) in [
        (json!({}), true, false, false),
        (
            json!({"head_lines": 0, "tail_lines": 1}),
            false,
            false,
            true,
        ),
        (json!({"tail_lines": 1}), true, false, true),
        (json!({"head_lines": 3}), true, true, true),
    ] {
        for result in invoke_both_paths(&server, &toolset, args).await {
            assert_ne!(result["isError"], true, "{result}");
            let text = assistant_text(&result);
            let stdout = text.split("<!-- start stdout -->").nth(1).unwrap();
            for (line, present) in [("first", first), ("middle", middle), ("last", last)] {
                assert_eq!(stdout.contains(line), present, "{text}");
            }
        }
    }
    toolset.cleanup_log_dir().await.unwrap();
}
