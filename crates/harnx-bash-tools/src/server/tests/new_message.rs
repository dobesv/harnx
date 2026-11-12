//! A new message for the caller, or the caller's run deadline drawing near,
//! ends `wait` and spawn's nested wait early, with the still-running result,
//! and never loses the process.
use super::process_wait::terminate_and_reap;
use super::*;
use crate::BashToolset;
use harnx_toolset::{PartialResultStore, ToolInvocation, ToolInvocationContext, Toolset};
use std::sync::Mutex as StdMutex;
use tokio_util::sync::CancellationToken;

/// Keeps every partial result a call records, in order.
#[derive(Default)]
struct Recorded(StdMutex<Vec<Value>>);

#[async_trait::async_trait]
impl PartialResultStore for Recorded {
    async fn record_partial_result(&self, value: Value) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(value);
        Ok(())
    }
}

/// A native invocation of `tool`, as the tool server would make it.
fn invocation(tool: &str, args: Value, context: ToolInvocationContext) -> ToolInvocation {
    ToolInvocation {
        tool: tool.into(),
        args,
        context,
        cancel: CancellationToken::new(),
    }
}

fn result_text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().expect("text result")
}

fn assert_stopped_running(text: &str) {
    assert_eq!(extract_field(text, "status"), "running", "{text}");
    assert_eq!(extract_field(text, "stopped_by"), "new_message", "{text}");
}

/// Fire `token` after `delay`, while the call it belongs to is waiting.
fn fire_after(token: &CancellationToken, delay: Duration) {
    let token = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        token.cancel();
    });
}

#[tokio::test]
async fn wait_stopped_by_new_message_reports_running_and_keeps_the_process() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let spawned = server
        .spawn_impl(spawn_params("sleep 30", directory.path()))
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&spawned), "execution_id");
    let new_message = CancellationToken::new();
    let context = ToolInvocationContext {
        new_message: new_message.clone(),
        ..Default::default()
    };
    fire_after(&new_message, Duration::from_millis(200));
    let started = std::time::Instant::now();
    let args = json!({"execution_id": execution_id, "seconds": 30});
    let result = BashToolset::from_server(server.clone())
        .invoke_with_context(invocation("wait", args, context))
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_stopped_running(result_text(&result));

    let status = server
        .status_impl(StatusParams {
            execution_id: execution_id.clone(),
            head_lines: None,
            tail_lines: None,
            max_output_bytes: None,
            grep: None,
        })
        .await
        .unwrap();
    assert_eq!(extract_field(&text_content(&status), "status"), "running");
    terminate_and_reap(&server, &execution_id).await;
}

/// A message that is already pending when spawn starts its nested wait ends
/// that wait at once. The partial result recorded before the wait names the
/// process, so a call that is then interrupted still hands back its id.
#[tokio::test]
async fn spawn_wait_stopped_by_new_message_keeps_its_execution_id() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let recorded = Arc::new(Recorded::default());
    let new_message = CancellationToken::new();
    new_message.cancel();
    let context = ToolInvocationContext {
        new_message,
        partial_result_store: Some(recorded.clone()),
        ..Default::default()
    };
    let args = json!({
        "command": "sleep 30",
        "working_dir": directory.path(),
        "wait": {"seconds": 30},
    });
    let result = BashToolset::from_server(server.clone())
        .invoke_with_context(invocation("spawn", args, context))
        .await
        .unwrap();
    let text = result_text(&result);
    assert_stopped_running(text);
    let execution_id = extract_field(text, "execution_id");
    let partial = recorded.0.lock().unwrap().clone();
    assert_eq!(
        partial,
        [json!({
            "execution_id": execution_id,
            "stdout_log_path": extract_field(text, "stdout_log_path"),
            "stderr_log_path": extract_field(text, "stderr_log_path"),
        })]
    );
    terminate_and_reap(&server, &execution_id).await;
}

#[test]
fn only_spawn_and_wait_stop_on_new_message() {
    let opted_in: Vec<String> = crate::builtin_tool_specs()
        .into_iter()
        .filter(|spec| spec.stops_on_new_message())
        .map(|spec| spec.name)
        .collect();
    assert_eq!(opted_in, ["spawn", "wait"]);
}

/// A context whose caller's run deadline is `in_secs` from now.
fn context_with_deadline(in_secs: i64) -> ToolInvocationContext {
    let deadline = chrono::Utc::now() + chrono::Duration::seconds(in_secs);
    ToolInvocationContext {
        run_context: Some(harnx_toolset::AutonomousRunContext {
            snapshot: json!({"deadline": deadline.to_rfc3339()}),
            started_at_ms: 0,
        }),
        ..Default::default()
    }
}

async fn spawn_sleeper(server: &BashServer, directory: &TestDir) -> String {
    let spawned = server
        .spawn_impl(spawn_params("sleep 30", directory.path()))
        .await
        .unwrap();
    extract_field(&text_content(&spawned), "execution_id")
}

#[tokio::test]
async fn forever_wait_stops_at_the_caller_deadline_and_keeps_the_process() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let execution_id = spawn_sleeper(&server, &directory).await;
    for in_secs in [61, -30] {
        let args = json!({"execution_id": execution_id, "forever": true});
        let started = std::time::Instant::now();
        let result = BashToolset::from_server(server.clone())
            .invoke_with_context(invocation("wait", args, context_with_deadline(in_secs)))
            .await
            .unwrap();
        // The bound is about a second away; the margin is for a slow runner.
        assert!(started.elapsed() < Duration::from_secs(20));
        let text = result_text(&result);
        assert_eq!(extract_field(text, "status"), "running", "{text}");
        assert_eq!(
            extract_field(text, "stopped_by"),
            "caller_deadline",
            "{text}"
        );
    }
    terminate_and_reap(&server, &execution_id).await;
}

#[tokio::test]
async fn forever_wait_without_a_run_context_has_no_deadline_clamp() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let execution_id = spawn_sleeper(&server, &directory).await;
    let args = json!({"execution_id": execution_id, "forever": true});
    let toolset = BashToolset::from_server(server.clone());
    let wait =
        toolset.invoke_with_context(invocation("wait", args, ToolInvocationContext::default()));
    let outcome = tokio::time::timeout(Duration::from_secs(3), wait).await;
    assert!(
        outcome.is_err(),
        "a wait with no deadline must keep waiting"
    );
    terminate_and_reap(&server, &execution_id).await;
}
