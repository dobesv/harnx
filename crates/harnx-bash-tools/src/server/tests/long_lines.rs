use super::*;
use harnx_core::safety::DEFAULT_MAX_BYTES;

fn long_line_command(length: usize) -> String {
    format!("printf 'HEAD%0{length}dTAIL\\n' 0; printf 'HEAD%0{length}dTAIL\\n' 0 >&2")
}

fn stream_body<'a>(text: &'a str, stream: &str) -> &'a str {
    text.split_once(&format!("<!-- start {stream} -->\n```\n"))
        .expect("stream start")
        .1
        .split_once(&format!("\n```\n<!-- end {stream} -->"))
        .expect("stream end")
        .0
}

fn assert_long_line_output(
    result: &CallToolResult,
    length: usize,
    max_output_bytes: Option<usize>,
    streams: usize,
) {
    assert_eq!(result.is_error, Some(false));
    let text = text_content(result);
    let line = format!("HEAD{}TAIL", "0".repeat(length));
    let (raw_output, outputs) = if streams == 1 {
        (
            format!("1: {line}"),
            vec![result.content[0].as_text().unwrap().text.as_str()],
        )
    } else {
        (
            format!("{line}\n"),
            vec![stream_body(&text, "stdout"), stream_body(&text, "stderr")],
        )
    };
    let cap = max_output_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let expected = truncate_output(
        &raw_output,
        &TruncateOpts {
            max_output_bytes: cap,
            ..Default::default()
        },
    );
    for output in outputs {
        // Server notices and markdown fences aren't part of the per-stream byte cap.
        let output = output.split("\n\n[").next().unwrap();
        assert_eq!(output, expected);
        assert!(output.len() <= cap);
        assert!(!output.contains("removed from line"));
        assert_eq!(output.contains(&line), raw_output.len() <= cap);
        assert_eq!(
            output.contains("bytes removed, showing first"),
            raw_output.len() > cap
        );
    }
}

#[tokio::test]
async fn exec_default_and_explicit_byte_budgets_bound_long_lines() {
    let dir = TestDir::new();
    let server = server_with_paths(vec![dir.path().to_path_buf()]);
    for length in [5000, 100_000, DEFAULT_MAX_BYTES + 4096] {
        for max_output_bytes in [None, Some(65536)] {
            let mut params = exec_params(long_line_command(length), dir.path());
            params.max_output_bytes = max_output_bytes;
            let result = server.exec_command_impl(params).await.unwrap();
            assert_long_line_output(&result, length, max_output_bytes, 2);
        }
    }
}

#[tokio::test]
async fn read_exec_log_default_and_explicit_byte_budgets_bound_long_lines() {
    let dir = TestDir::new();
    let server = server_with_paths(vec![dir.path().to_path_buf()]);
    for length in [5000, 100_000, DEFAULT_MAX_BYTES + 4096] {
        let result = server
            .exec_command_impl(exec_params(long_line_command(length), dir.path()))
            .await
            .unwrap();
        assert_long_line_output(&result, length, None, 2);
        let execution_id = extract_field(&text_content(&result), "execution_id");
        for stream in ["stdout", "stderr"] {
            for max_output_bytes in [None, Some(65536)] {
                let mut params = read_exec_log_params(&execution_id, stream);
                params.max_output_bytes = max_output_bytes;
                let result = server.read_exec_log_impl(params).await.unwrap();
                assert_long_line_output(&result, length, max_output_bytes, 1);
            }
        }
    }
}

#[tokio::test]
async fn spawn_wait_status_default_and_explicit_byte_budgets_bound_long_lines() {
    let dir = TestDir::new();
    let server = server_with_paths(vec![dir.path().to_path_buf()]);
    for length in [5000, 100_000, DEFAULT_MAX_BYTES + 4096] {
        for max_output_bytes in [None, Some(65536)] {
            let mut params = spawn_params(long_line_command(length), dir.path());
            params.wait = Some(WaitOptions {
                seconds: Some(15),
                max_output_bytes,
                ..Default::default()
            });
            let result = server.spawn_impl(params).await.unwrap();
            assert_long_line_output(&result, length, max_output_bytes, 2);
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
                assert_long_line_output(&result, length, budget, 2);
                let result = server
                    .status_impl(StatusParams {
                        diff: None,
                        execution_id: execution_id.clone(),
                        head_lines: None,
                        tail_lines: None,
                        max_output_bytes: budget,
                        grep: None,
                    })
                    .await
                    .unwrap();
                assert_long_line_output(&result, length, budget, 2);
            }
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
        assert!(output.contains("Use max_output_bytes"));
    }
}
