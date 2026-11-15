mod defaults;

use super::*;
use harnx_core::safety::DEFAULT_MAX_BYTES;

fn long_lines(length: usize) -> String {
    format!("printf 'HEAD%0{length}dTAIL\\n' 0; printf 'HEAD%0{length}dTAIL\\n' 0 >&2")
}

async fn template_tools(root: &Path, script: &str) -> (BashServer, BashToolset) {
    let path = root.join("output.yaml");
    write_template(&path, "output", "", script);
    let templates = discover_tool_templates(None, &[path], &[]).unwrap();
    let server =
        BashServer::new_with_templates(sandbox_config_for(root), templates.clone()).unwrap();
    let toolset = BashToolset::new(sandbox_config_for(root), templates)
        .await
        .unwrap();
    (server, toolset)
}

fn assistant_text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

fn assert_template_long_lines(result: &Value, length: usize, full: bool) {
    assert_ne!(result["isError"], true);
    let text = assistant_text(result);
    let line = format!("HEAD{}TAIL", "0".repeat(length));
    assert_eq!(text.matches(&line).count(), if full { 2 } else { 0 });
    assert_eq!(text.matches("HEAD00000000000000000000").count(), 2);
    assert_eq!(text.matches("00000000000000000000TAIL").count(), 2);
    assert!(!text.contains("removed from line"));
    assert_eq!(text.contains("bytes removed, showing first"), !full);
    assert!(!text.contains("read_exec_log"), "{text}");
    if !full {
        assert!(
            text.contains("Use max_output_bytes, head_lines, or tail_lines to see more]"),
            "{text}"
        );
    }
    assert!(text.contains("execution_id:"), "{text}");
    assert!(text.contains("stdout_log_path:"), "{text}");
}

#[tokio::test]
async fn template_output_controls_recover_long_lines_on_native_and_mcp_paths() {
    for length in [5000, DEFAULT_MAX_BYTES + 4096] {
        let dir = tempfile::tempdir().unwrap();
        let command = long_lines(length);
        let (server, toolset) = template_tools(dir.path(), &command).await;
        for (args, full) in [
            (json!({}), length < DEFAULT_MAX_BYTES),
            (json!({"max_output_bytes": length + 1024}), true),
        ] {
            let result = toolset
                .invoke("output", args.clone(), CancellationToken::new())
                .await
                .unwrap();
            assert_template_long_lines(&result, length, full);
            let result = mcp_call_template(server.clone(), "output", args)
                .await
                .unwrap();
            assert_template_long_lines(&result, length, full);
        }
        let result = toolset
            .invoke(
                "exec",
                json!({"command": command, "max_output_bytes": 1024}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let text = assistant_text(&result);
        assert!(text.contains("Use max_output_bytes, head_lines, or tail_lines to see more; full log via read_exec_log: execution_id="), "{text}");
        toolset.cleanup_log_dir().await.unwrap();
    }
}

#[tokio::test]
async fn template_output_controls_apply_head_tail_windows() {
    let dir = tempfile::tempdir().unwrap();
    let toolset = template_tools(dir.path(), "printf 'first\\nmiddle\\nlast\\n'")
        .await
        .1;
    for (args, kept, dropped) in [
        (json!({"head_lines": 1, "tail_lines": 0}), "first", "last"),
        (json!({"head_lines": 0, "tail_lines": 1}), "last", "first"),
    ] {
        let result = toolset
            .invoke("output", args, CancellationToken::new())
            .await
            .unwrap();
        let text = assistant_text(&result);
        let stdout = text.split("<!-- start stdout -->").nth(1).unwrap();
        assert!(stdout.contains(kept), "{text}");
        assert!(!stdout.contains(dropped), "{text}");
        assert!(!stdout.contains("middle"), "{text}");
        assert!(!stdout.contains("read_exec_log"), "{text}");
    }
    toolset.cleanup_log_dir().await.unwrap();
}

#[tokio::test]
async fn template_output_controls_do_not_enter_script_environment() {
    let dir = tempfile::tempdir().unwrap();
    let toolset = template_tools(dir.path(), "printf 'controls=%s:%s:%s\\n' \"${HEAD_LINES-unset}\" \"${TAIL_LINES-unset}\" \"${MAX_OUTPUT_BYTES-unset}\"").await.1;
    let result = toolset
        .invoke(
            "output",
            json!({"head_lines": 1, "tail_lines": 1, "max_output_bytes": 65536}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        assistant_text(&result).contains("controls=unset:unset:unset"),
        "{result}"
    );
    toolset.cleanup_log_dir().await.unwrap();
}

#[tokio::test]
async fn template_output_controls_return_invalid_params_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    let toolset = template_tools(dir.path(), "touch ran.marker").await.1;
    for name in ["head_lines", "tail_lines", "max_output_bytes"] {
        for bad in [
            json!(-1),
            json!(1.5),
            json!("4"),
            json!(true),
            json!([]),
            json!({}),
        ] {
            let error = toolset
                .invoke("output", json!({name: bad}), CancellationToken::new())
                .await
                .unwrap_err();
            assert!(matches!(error, ToolInvokeError::Recoverable(_)), "{error}");
            assert!(
                error
                    .to_string()
                    .contains(&format!("invalid type for parameter `{name}`:")),
                "{error}"
            );
        }
    }
    assert!(!dir.path().join("ran.marker").exists());
    toolset.cleanup_log_dir().await.unwrap();
}

#[tokio::test]
async fn template_timeout_truncation_uses_output_controls_only() {
    let dir = tempfile::tempdir().unwrap();
    let command = long_lines(DEFAULT_MAX_BYTES + 4096);
    let toolset = template_tools(dir.path(), &format!("{command}; sleep 30"))
        .await
        .1;
    let result = toolset
        .invoke(
            "output",
            json!({"timeout_secs": 1}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["isError"], true, "{result}");
    let text = assistant_text(&result);
    assert!(text.contains("timed out after 1s"), "{text}");
    assert!(!text.contains("removed from line"));
    assert!(text.contains("bytes removed, showing first"));
    assert!(text.contains("HEAD00000000000000000000"));
    assert!(text.contains("00000000000000000000TAIL"));
    assert!(
        text.contains("Use max_output_bytes, head_lines, or tail_lines to see more]"),
        "{text}"
    );
    assert!(!text.contains("read_exec_log"), "{text}");
    toolset.cleanup_log_dir().await.unwrap();
}
