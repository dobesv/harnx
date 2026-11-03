use super::*;

pub(super) fn assert_backend_capture_notice(text: &str) {
    assert!(text.contains("backend capture limit"), "{text}");
    assert!(!text.contains("Use max_output_bytes"), "{text}");
    assert!(!text.contains("head_lines, or tail_lines"), "{text}");
}

#[tokio::test]
async fn presentation_truncation_keeps_output_control_guidance() {
    let (bash, _slot, _hooks, _ctx) = harness(FakeProvider::new(), Vec::new());
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
    let (bash, _slot, _hooks, _ctx) = harness(Arc::new(FatalProvider), Vec::new());
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
