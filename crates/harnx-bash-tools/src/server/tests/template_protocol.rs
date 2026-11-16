use super::template_new_message::{result_text, server, template, toolset};
use super::*;
use crate::server::protocol_reader::test_support::Recorder;
use harnx_toolset::{ToolInvocation, ToolInvocationContext, ToolProgressHandle, Toolset};
use tokio_util::sync::CancellationToken;

/// A script line that reports progress titled [`PROGRESS_TITLE`].
const PROGRESS_LINE: &str = r#"printf '%s\n' '{"progress":{"title":"working"}}'"#;
const PROGRESS_TITLE: &str = "working";

fn invocation(tool: &str, args: Value, context: ToolInvocationContext) -> ToolInvocation {
    ToolInvocation {
        tool: tool.into(),
        args,
        context,
        cancel: CancellationToken::new(),
    }
}

fn context_with(
    recorder: &Arc<Recorder>,
    new_message: &CancellationToken,
) -> ToolInvocationContext {
    ToolInvocationContext {
        progress: ToolProgressHandle::new(recorder.clone()),
        new_message: new_message.clone(),
        ..Default::default()
    }
}

/// Runs a `protocol: jsonl` template to completion and returns its result.
async fn run_protocol(directory: &TestDir, script: &str, args: Value) -> Value {
    let recorder = Arc::new(Recorder::default());
    toolset(directory, template("protocol: jsonl", script))
        .invoke_with_context(invocation(
            "test_template",
            args,
            context_with(&recorder, &CancellationToken::new()),
        ))
        .await
        .unwrap()
}

/// The text between a stream's start and end markers, if the result has that
/// stream. The metadata header repeats the script, so assertions about output
/// must look inside the block.
fn stream_block<'a>(text: &'a str, stream: &str) -> Option<&'a str> {
    let start = format!("<!-- start {stream} -->");
    let end = format!("<!-- end {stream} -->");
    let rest = &text[text.find(&start)? + start.len()..];
    Some(&rest[..rest.find(&end)?])
}

fn has_progress(recorder: &Recorder) -> bool {
    recorder
        .patches()
        .iter()
        .any(|patch| patch.title.as_deref() == Some(PROGRESS_TITLE))
}

async fn wait_for_progress(recorder: &Recorder) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !has_progress(recorder) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("progress should arrive while the script is still running");
}

#[tokio::test]
async fn protocol_env_is_set_only_for_protocol_templates() {
    let directory = TestDir::new();
    let script = r#"echo "protocol=${HARNX_TOOL_PROTOCOL:-unset}" >&2"#;
    for (yaml, expected) in [
        ("protocol: jsonl", "protocol=jsonl"),
        ("", "protocol=unset"),
    ] {
        let toolset = toolset(&directory, template(yaml, script));
        let result = toolset
            .invoke_with_context(invocation("test_template", json!({}), Default::default()))
            .await
            .unwrap();
        assert!(result_text(&result).contains(expected), "{result}");
    }
}

#[tokio::test]
async fn progress_arrives_while_the_script_is_still_running() {
    let directory = TestDir::new();
    let script = format!(
        "{PROGRESS_LINE}\n\
         for _ in $(seq 200); do [ -f \"$PWD/go\" ] && break; sleep 0.05; done\n\
         [ -f \"$PWD/go\" ] || {{ echo \"never released\" >&2; exit 3; }}\n\
         echo done >&2"
    );
    let toolset = toolset(&directory, template("protocol: jsonl", &script));
    let recorder = Arc::new(Recorder::default());
    let call = invocation(
        "test_template",
        json!({}),
        context_with(&recorder, &CancellationToken::new()),
    );
    let task = tokio::spawn(async move { toolset.invoke_with_context(call).await });

    wait_for_progress(&recorder).await;
    std::fs::write(directory.path().join("go"), "").unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("released script should finish")
        .unwrap()
        .unwrap();
    let text = result_text(&result);
    assert_eq!(extract_field(text, "exit_code"), "0", "{text}");
    let stderr = stream_block(text, "stderr").expect("stderr block");
    assert!(stderr.contains("done"), "{text}");
    assert!(!stderr.contains("never released"), "{text}");
}

#[tokio::test]
async fn protocol_result_contains_only_stderr() {
    let directory = TestDir::new();
    let script = format!("{PROGRESS_LINE}\necho out-line\necho err-line >&2");
    let result = run_protocol(&directory, &script, json!({})).await;
    let text = result_text(&result);
    assert!(
        stream_block(text, "stderr").is_some_and(|block| block.contains("err-line")),
        "{text}"
    );
    assert!(!text.contains("<!-- start stdout -->"), "{text}");
    assert!(!text.contains("\nout-line"), "{text}");
    assert_eq!(extract_field(text, "total_lines"), "1", "{text}");
    let stdout_log = std::fs::read_to_string(extract_field(text, "stdout_log_path")).unwrap();
    assert!(stdout_log.contains("out-line"), "{stdout_log}");
}

#[tokio::test]
async fn empty_stderr_returns_header_only() {
    let directory = TestDir::new();
    let result = run_protocol(&directory, PROGRESS_LINE, json!({})).await;
    let text = result_text(&result);
    assert_eq!(extract_field(text, "exit_code"), "0", "{text}");
    assert_eq!(extract_field(text, "total_lines"), "0", "{text}");
    assert!(text.contains("<!-- start stderr -->\n```\n\n```"), "{text}");
    assert!(!text.contains("<!-- start stdout -->"), "{text}");
}

#[tokio::test]
async fn output_controls_apply_to_stderr() {
    let directory = TestDir::new();
    // The window is head plus tail, so the default 250 head lines would keep
    // all three lines.
    let result = run_protocol(
        &directory,
        r"printf 'a\nb\nc\n' >&2",
        json!({"head_lines": 0, "tail_lines": 1}),
    )
    .await;
    let text = result_text(&result);
    let stderr = stream_block(text, "stderr").expect("stderr block");
    assert!(stderr.contains("\nc\n"), "{text}");
    assert!(!stderr.contains("\na\n"), "{text}");
    assert!(!stderr.contains("\nb\n"), "{text}");
    assert!(stream_block(text, "stdout").is_none(), "{text}");
}

#[tokio::test]
async fn nonzero_exit_returns_stderr() {
    let directory = TestDir::new();
    let result = run_protocol(&directory, "echo boom >&2; exit 4", json!({})).await;
    let text = result_text(&result);
    assert_eq!(extract_field(text, "exit_code"), "4", "{text}");
    assert!(
        stream_block(text, "stderr").is_some_and(|block| block.contains("boom")),
        "{text}"
    );
    assert!(stream_block(text, "stdout").is_none(), "{text}");
}

#[tokio::test]
async fn timeout_result_contains_only_stderr() {
    let directory = TestDir::new();
    let script = format!("{PROGRESS_LINE}\necho late >&2\nsleep 30");
    let result = run_protocol(&directory, &script, json!({"timeout_secs": 1})).await;
    assert_eq!(result["isError"], true, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("timed out after 1s"), "{text}");
    assert!(
        stream_block(text, "stderr").is_some_and(|block| block.contains("late")),
        "{text}"
    );
    assert!(!text.contains("<!-- start stdout -->"), "{text}");
}

#[tokio::test]
async fn new_message_keeps_progress_and_partial_stderr() {
    let directory = TestDir::new();
    // stderr is written before the progress line, so once the progress is
    // seen the partial output is already in the pipe.
    let script = format!("echo partial >&2\n{PROGRESS_LINE}\nsleep 30");
    let toolset = toolset(
        &directory,
        template("protocol: jsonl\non_user_message: terminate", &script),
    );
    let recorder = Arc::new(Recorder::default());
    let token = CancellationToken::new();
    let call = invocation("test_template", json!({}), context_with(&recorder, &token));
    let task = tokio::spawn(async move { toolset.invoke_with_context(call).await });

    wait_for_progress(&recorder).await;
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("new message should stop the template promptly")
        .unwrap()
        .unwrap();
    let text = result_text(&result);
    assert_eq!(extract_field(text, "stopped_by"), "new_message", "{text}");
    assert!(
        stream_block(text, "stderr").is_some_and(|block| block.contains("partial")),
        "{text}"
    );
    assert!(has_progress(&recorder));
}

#[tokio::test]
async fn mcp_route_discards_progress_and_returns_stderr() {
    let directory = TestDir::new();
    let script = format!("{PROGRESS_LINE}\necho ok >&2");
    let server = server(&directory, template("protocol: jsonl", &script));
    let result = server
        .invoke_tool_value("test_template", json!({}))
        .await
        .expect("MCP route should succeed without a progress sink");
    let result = serde_json::to_value(result).unwrap();
    let text = result_text(&result);
    assert!(
        stream_block(text, "stderr").is_some_and(|block| block.contains("ok")),
        "{text}"
    );
    assert!(!text.contains("<!-- start stdout -->"), "{text}");
}

#[tokio::test]
async fn plain_template_result_is_unchanged() {
    let directory = TestDir::new();
    let toolset = toolset(&directory, template("", "echo out\necho err >&2"));
    let result = toolset
        .invoke_with_context(invocation("test_template", json!({}), Default::default()))
        .await
        .unwrap();
    let text = result_text(&result);
    assert!(
        stream_block(text, "stdout").is_some_and(|block| block.contains("\nout\n")),
        "{text}"
    );
    assert!(
        stream_block(text, "stderr").is_some_and(|block| block.contains("\nerr\n")),
        "{text}"
    );
    assert_eq!(extract_field(text, "total_lines"), "2", "{text}");
}
