use super::*;

const LONG_LINE_COMMAND: &str = "printf '%05000d\\n' 0; printf '%05000d\\n' 0 >&2";

fn assert_long_line_output(result: &CallToolResult, full: bool, streams: usize) {
    assert_eq!(result.is_error, Some(false));
    let text = text_content(result);
    if full {
        assert_eq!(text.matches(&"0".repeat(5000)).count(), streams, "{text}");
        assert!(!text.contains("removed from line"), "{text}");
    } else {
        assert_eq!(text.matches("removed from line").count(), streams, "{text}");
        assert!(!text.contains(&"0".repeat(5000)), "{text}");
    }
}

#[tokio::test]
async fn exec_explicit_byte_budget_recovers_long_lines() {
    let dir = TestDir::new();
    let server = server_with_paths(vec![dir.path().to_path_buf()]);
    for max_output_bytes in [None, Some(65536)] {
        let mut params = exec_params(LONG_LINE_COMMAND, dir.path());
        params.max_output_bytes = max_output_bytes;
        let result = server.exec_command_impl(params).await.unwrap();
        assert_long_line_output(&result, max_output_bytes.is_some(), 2);
    }
}

#[tokio::test]
async fn read_exec_log_explicit_byte_budget_recovers_long_lines() {
    let dir = TestDir::new();
    let server = server_with_paths(vec![dir.path().to_path_buf()]);
    let result = server
        .exec_command_impl(exec_params(LONG_LINE_COMMAND, dir.path()))
        .await
        .unwrap();
    assert_long_line_output(&result, false, 2);
    let execution_id = extract_field(&text_content(&result), "execution_id");
    for stream in ["stdout", "stderr"] {
        for max_output_bytes in [None, Some(65536)] {
            let mut params = read_exec_log_params(&execution_id, stream);
            params.max_output_bytes = max_output_bytes;
            let result = server.read_exec_log_impl(params).await.unwrap();
            assert_long_line_output(&result, max_output_bytes.is_some(), 1);
        }
    }
}

#[tokio::test]
async fn spawn_wait_status_explicit_byte_budget_recovers_long_lines() {
    let dir = TestDir::new();
    let server = server_with_paths(vec![dir.path().to_path_buf()]);
    for max_output_bytes in [None, Some(65536)] {
        let mut params = spawn_params(LONG_LINE_COMMAND, dir.path());
        params.wait = Some(WaitOptions {
            seconds: Some(15),
            max_output_bytes,
            ..Default::default()
        });
        let result = server.spawn_impl(params).await.unwrap();
        assert_long_line_output(&result, max_output_bytes.is_some(), 2);
        let execution_id = extract_field(&text_content(&result), "execution_id");
        // Re-read the same completed execution with both budgets: output controls
        // belong to the observation, not to the process or its persisted logs.
        for budget in [None, Some(65536)] {
            let result = server
                .wait_impl(wait_params_with(
                    execution_id.clone(),
                    WaitOptions {
                        seconds: Some(15),
                        max_output_bytes: budget,
                        ..Default::default()
                    },
                ))
                .await
                .unwrap();
            assert_long_line_output(&result, budget.is_some(), 2);
            let result = server
                .status_impl(StatusParams {
                    execution_id: execution_id.clone(),
                    head_lines: None,
                    tail_lines: None,
                    max_output_bytes: budget,
                    grep: None,
                })
                .await
                .unwrap();
            assert_long_line_output(&result, budget.is_some(), 2);
        }
    }
}

#[test]
fn bash_truncate_opts_preserve_defaults_and_explicit_budget_bounds() {
    let long_line = "x".repeat(5000);
    let text = format!("{long_line}\n").repeat(150);
    let opts = BashServer::truncate_opts_from(None, None, None);
    assert_eq!(
        truncate_output(&text, &opts),
        truncate_output(&text, &TruncateOpts::default())
    );
    let opts = BashServer::truncate_opts_from(Some(1), Some(1), Some(65536));
    let output = truncate_output(&text, &opts);
    assert_eq!(output.matches(&long_line).count(), 2);
    assert!(!output.contains("removed from line"));
    for cap in [0, 1024] {
        let opts = BashServer::truncate_opts_from(None, None, Some(cap));
        let output = truncate_output(&long_line, &opts);
        assert_eq!(opts.line_head_bytes, 0);
        assert_eq!(opts.line_tail_bytes, 0);
        // Core's marker can exceed tiny byte caps; keep that behavior unchanged.
        if cap > 0 {
            assert!(output.len() <= cap);
        }
        assert!(!output.contains("removed from line"));
    }
}
