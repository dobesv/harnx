use super::*;

pub(super) fn assert_backend_capture_notice(text: &str) {
    assert!(text.contains("backend capture limit"), "{text}");
    assert!(!text.contains("Use max_output_bytes"), "{text}");
    assert!(!text.contains("head_lines, or tail_lines"), "{text}");
}

#[tokio::test]
async fn presentation_truncation_keeps_output_control_guidance() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let output = ok(bash
        .call_tool(
            "bash",
            json!({"script": "echo abcdefghijklmnop", "max_output_bytes": 3}),
            &create_abort_signal(),
        )
        .await);
    assert_eq!(output.value["stdout_truncated"], true);
    assert!(output.value["limit_hit"].is_null());
    let text = output.value["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Use max_output_bytes, head_lines, or tail_lines"),
        "{text}"
    );
    assert!(!text.contains("backend capture limit"), "{text}");
}

#[tokio::test]
async fn bash_default_and_explicit_byte_budgets_bound_long_lines() {
    use harnx_core::safety::DEFAULT_MAX_BYTES;

    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    for length in [5000, 100_000, (DEFAULT_MAX_BYTES / 5000 + 1) * 5000] {
        let line = format!("HEAD{}TAIL", "0".repeat(length));
        for max_output_bytes in [None, Some(65536)] {
            // BashKit bounds printf field widths, so build oversized lines in chunks.
            let chunks = length / 5000;
            let mut params = json!({
                "script": format!("printf HEAD; printf HEAD >&2; for i in $(seq 1 {chunks}); do printf '%05000d' 0; printf '%05000d' 0 >&2; done; printf 'TAIL\\n'; printf 'TAIL\\n' >&2")
            });
            if let Some(cap) = max_output_bytes {
                params["max_output_bytes"] = json!(cap);
            }
            let output = ok(bash.call_tool("bash", params, &create_abort_signal()).await);
            let cap = max_output_bytes.unwrap_or(DEFAULT_MAX_BYTES);
            let full = line.len() < cap;
            for stream in ["stdout", "stderr"] {
                let text = output.value[stream].as_str().unwrap();
                assert_eq!(text.contains(&line), full, "{stream}");
                assert!(!text.contains("removed from line"), "{stream}");
                assert_eq!(output.value[format!("{stream}_truncated")], !full);
                assert!(text.starts_with("HEAD"), "{stream}");
                assert!(text.ends_with("TAIL\n"), "{stream}");
                assert!(text.len() <= cap, "{stream}");
                assert_eq!(text.contains("bytes removed, showing first"), !full);
                let expected = truncate_output(
                    &format!("{line}\n"),
                    &TruncateOpts {
                        max_output_bytes: cap,
                        ..Default::default()
                    },
                );
                assert_eq!(text, expected);
            }
            let report = output.value["content"][0]["text"].as_str().unwrap();
            assert_eq!(report.matches(&line).count(), if full { 2 } else { 0 });
            assert!(!report.contains("removed from line"));
            assert_eq!(report.contains("bytes removed, showing first"), !full);
        }
    }
}

#[tokio::test]
async fn bash_explicit_byte_budget_keeps_line_window_and_byte_cap() {
    let (bash, _slot, _ctx) = harness(FakeProvider::new(), Vec::new());
    let output = ok(bash
        .call_tool(
            "bash",
            json!({
                "script": "printf '%05000d\\n' 0; echo omitted-middle; printf '%05000d\\n' 0",
                "head_lines": 1, "tail_lines": 1, "max_output_bytes": 65536
            }),
            &create_abort_signal(),
        )
        .await);
    let text = output.value["stdout"].as_str().unwrap();
    assert_eq!(text.matches(&"0".repeat(5000)).count(), 2);
    assert!(!text.contains("omitted-middle"));
    assert!(!text.contains("removed from line"));
    for cap in [0, 1024] {
        let output = ok(bash
            .call_tool(
                "bash",
                json!({"script": "printf '%05000d\\n' 0", "max_output_bytes": cap}),
                &create_abort_signal(),
            )
            .await);
        let text = output.value["stdout"].as_str().unwrap();
        // Core's marker can exceed tiny byte caps; keep that behavior unchanged.
        if cap > 0 {
            assert!(text.len() <= cap as usize);
        }
        assert!(!text.contains("removed from line"));
        assert_eq!(output.value["stdout_truncated"], true);
    }
}

struct FatalProvider;

#[async_trait::async_trait]
impl ToolProvider for FatalProvider {
    fn name(&self) -> &str {
        "fatal-fixture"
    }

    fn has_tool(&self, name: &str) -> bool {
        name == "broken"
    }

    async fn call_tool(
        &self,
        _name: &str,
        _arguments: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        Err(ToolError::Fatal(anyhow!("fatal fixture")))
    }
}

#[tokio::test]
async fn backend_error_reaches_failure_payload_when_stderr_is_full() {
    let (bash, _slot, _ctx) = harness(Arc::new(FatalProvider), Vec::new());
    let error = recoverable(
        bash.call_tool(
            "bash",
            json!({"script": "for i in $(seq 1 110); do printf '%010000d' 0 >&2; done\nbroken --path x", "max_output_bytes": 3}),
            &create_abort_signal(),
        )
        .await,
        "fatal command",
    );
    let payload = partial_result_of(&error).expect("failure carries partial result");
    assert_eq!(payload["exit_code"], 1);
    assert_eq!(payload["stderr_truncated"], true);
    let diagnostic = payload["error"]
        .as_str()
        .expect("diagnostic outside stream cap");
    assert!(diagnostic.contains("fatal fixture"), "{diagnostic}");
    let text = payload["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("fatal fixture"), "{text}");
    assert_backend_capture_notice(text);
}

#[test]
fn truncation_notices_are_scoped_to_each_stream_and_capture_wins() {
    for name in ["stdout", "stderr"] {
        let captured = render_stream_block(name, "prefix", true, true);
        assert!(captured.contains(&format!("{name} truncated at the backend capture limit")));
        assert!(!captured.contains("max_output_bytes"));
        let presented = render_stream_block(name, "prefix", true, false);
        assert!(presented.contains("Use max_output_bytes, head_lines, or tail_lines"));
        assert!(!render_stream_block(name, "all", false, false).contains("truncated"));
    }
}
