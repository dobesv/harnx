use super::*;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::ExitStatus;

fn spawn_with_wait(command: &str, directory: &Path, options: WaitOptions) -> SpawnCommandParams {
    SpawnCommandParams {
        wait: Some(options),
        ..spawn_params(command, directory)
    }
}

fn wait_options(timeout_secs: u64) -> WaitOptions {
    WaitOptions {
        timeout_secs: Some(timeout_secs),
        ..Default::default()
    }
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }

    fn kept_line(self, number: u8) -> String {
        let prefix = match self {
            Self::Stdout => "out",
            Self::Stderr => "err",
        };
        format!("keep-{prefix}-{number}")
    }

    fn byte_suffix(self) -> &'static str {
        match self {
            Self::Stdout => "yz",
            Self::Stderr => "YZ",
        }
    }

    fn full_payload(self) -> &'static str {
        match self {
            Self::Stdout => "abcdefghijklmnopqrstuvwxyz",
            Self::Stderr => "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
        }
    }

    fn opposite_payload(self) -> &'static str {
        match self {
            Self::Stdout => OutputStream::Stderr.full_payload(),
            Self::Stderr => OutputStream::Stdout.full_payload(),
        }
    }
}

const OUTPUT_STREAMS: [OutputStream; 2] = [OutputStream::Stdout, OutputStream::Stderr];

struct OutputText<'a>(&'a str);

impl std::ops::Deref for OutputText<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

struct LogPaths {
    stdout: String,
    stderr: String,
}

impl OutputText<'_> {
    fn block(&self, stream: OutputStream) -> &str {
        let name = stream.name();
        self.0
            .split_once(&format!("<!-- start {name} -->"))
            .unwrap()
            .1
            .split_once(&format!("<!-- end {name} -->"))
            .unwrap()
            .0
    }

    fn field(&self, field: &str) -> String {
        extract_field(self.0, field)
    }
}

fn stream_block<'a>(text: &'a OutputText<'a>, stream: OutputStream) -> &'a str {
    text.block(stream)
}

fn assert_line_filter_kept_edges(block: &str, stream: OutputStream) {
    assert!(block.contains(&stream.kept_line(1)), "{block}");
    assert!(block.contains(&stream.kept_line(4)), "{block}");
}

fn assert_line_filter_removed_middle(block: &str, stream: OutputStream) {
    assert!(!block.contains(&stream.kept_line(2)), "{block}");
    assert!(!block.contains("drop-"), "{block}");
}

fn assert_truncation_and_raw_log(text: &OutputText<'_>, block: &str, stream: OutputStream) {
    assert!(
        block.contains(&format!("{} truncated from", stream.name())),
        "{block}"
    );
    let log_path = text.field(&format!("{}_log_path", stream.name()));
    assert!(block.contains(&log_path));
    assert!(std::fs::read_to_string(log_path).unwrap().contains("drop-"));
}

fn assert_filtered_line_stream(text: &OutputText<'_>, stream: OutputStream) {
    let block = stream_block(text, stream);
    assert_line_filter_kept_edges(block, stream);
    assert_line_filter_removed_middle(block, stream);
    assert_truncation_and_raw_log(text, block, stream);
}

fn assert_byte_truncated_streams(text: &OutputText<'_>) {
    for stream in OUTPUT_STREAMS {
        let block = stream_block(text, stream);
        assert!(block.contains(stream.byte_suffix()), "{block}");
        assert!(!block.contains(stream.full_payload()), "{block}");
        assert!(!block.contains(stream.opposite_payload()), "{block}");
        assert!(
            block.contains(&format!("{} truncated from", stream.name())),
            "{block}"
        );
    }
}

fn assert_running_wait_response(text: &OutputText<'_>) {
    assert_eq!(extract_field(text, "status"), "running");
    assert_execution_metadata_fields(text);
    assert!(text.contains("running_for:"));
}

fn assert_spawn_logs_exist(text: &OutputText<'_>) -> LogPaths {
    let paths = LogPaths {
        stdout: extract_field(text, "stdout_log_path"),
        stderr: extract_field(text, "stderr_log_path"),
    };
    assert!(Path::new(&paths.stdout).exists());
    assert!(Path::new(&paths.stderr).exists());
    paths
}

fn assert_completed_wait_status(text: &OutputText<'_>) {
    assert_eq!(extract_field(text, "status"), "exited");
}

fn assert_completed_wait_paths(text: &OutputText<'_>, paths: &LogPaths) {
    assert_eq!(extract_field(text, "stdout_log_path"), paths.stdout);
    assert_eq!(extract_field(text, "stderr_log_path"), paths.stderr);
}

fn assert_completed_exit(text: &OutputText<'_>, exit_code: i32) {
    assert_completed_wait_status(text);
    assert_eq!(extract_field(text, "exit_code"), exit_code.to_string());
}

fn assert_completed_stream_output(text: &OutputText<'_>) {
    assert!(stream_block(text, OutputStream::Stdout).contains("completed"));
    assert!(stream_block(text, OutputStream::Stderr).contains("diagnostic"));
}

async fn assert_invalid_spawn_side_effects(directory: &TestDir, server: &BashServer) {
    assert!(!directory.path().join("must-not-exist").exists());
    assert!(!server.inner.log_dir.exists());
    assert!(server.inner.spawned.lock().await.is_empty());
}

fn assert_diff_commit_and_path(diff: &str) {
    assert!(diff.starts_with("commit "), "{diff}");
    assert!(diff.contains("note.txt"), "{diff}");
}

fn assert_diff_content(diff: &str) {
    assert!(diff.contains("-before"), "{diff}");
    assert!(diff.contains("+after"), "{diff}");
}

fn assert_diff_closed_fence(diff: &str) {
    assert!(diff.trim_end().ends_with("```"), "{diff}");
}

fn snapshot_diff(result: &CallToolResult) -> &str {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(text) if text.text.contains("```diff") => Some(&text.text),
            _ => None,
        })
        .next()
        .expect("existing diff content block must be returned")
}

async fn assert_running_process_can_finish(
    server: &BashServer,
    directory: &TestDir,
    execution_id: &str,
    initial_runtime: &str,
) {
    assert!(!initial_runtime.is_empty());
    let started_at = server.inner.spawned.lock().await[execution_id].started_at;
    // Shift the timestamp instead of sleeping to test accumulated running time.
    server
        .inner
        .spawned
        .lock()
        .await
        .get_mut(execution_id)
        .unwrap()
        .started_at = started_at - Duration::from_secs(10);

    let poll = text_content(
        &server
            .wait_impl(WaitParams {
                execution_id: execution_id.to_owned(),
                options: wait_options(0),
            })
            .await
            .unwrap(),
    );
    assert!(extract_field(&poll, "running_for").ends_with('s'));
    assert_eq!(
        server.inner.spawned.lock().await[execution_id].started_at,
        started_at - Duration::from_secs(10)
    );
    assert_ne!(extract_field(&poll, "running_for"), initial_runtime);

    std::fs::write(directory.path().join("release"), "").unwrap();
    let completed = text_content(&finish(server, execution_id).await);
    assert_eq!(extract_field(&completed, "status"), "exited");
    assert!(stream_block(&OutputText(&completed), OutputStream::Stdout).contains("keep-out-2"));
}

async fn finish(server: &BashServer, execution_id: &str) -> CallToolResult {
    server
        .wait_impl(WaitParams {
            execution_id: execution_id.to_string(),
            options: wait_options(5),
        })
        .await
        .unwrap()
}

async fn terminate_and_reap(server: &BashServer, execution_id: &str) {
    let result = server
        .terminate_impl(TerminateParams {
            execution_id: execution_id.to_string(),
            signal: Some("SIGTERM".to_string()),
        })
        .await
        .unwrap();
    assert_eq!(
        extract_field(&text_content(&result), "status"),
        "terminated"
    );
    let result = finish(server, execution_id).await;
    assert_eq!(extract_field(&text_content(&result), "status"), "exited");
    assert!(!server.inner.spawned.lock().await.contains_key(execution_id));
}

#[tokio::test]
async fn omitted_wait_keeps_spawn_only_result() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let command = "echo spawn-only";
    let result = server
        .spawn_impl(spawn_params(command, directory.path()))
        .await
        .unwrap();
    let text = text_content(&result);
    let execution_id = extract_field(&text, "execution_id");
    let stdout = PathBuf::from(extract_field(&text, "stdout_log_path"));
    let stderr = PathBuf::from(extract_field(&text, "stderr_log_path"));
    let working_dir = directory.path().canonicalize().unwrap();
    let expected = BashServer::build_spawn_result(SpawnResultCtx {
        execution_id: &execution_id,
        command,
        working_dir: &working_dir,
        stdout_log_path: &stdout,
        stderr_log_path: &stderr,
    });
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert!(server
        .inner
        .spawned
        .lock()
        .await
        .contains_key(&execution_id));
    finish(&server, &execution_id).await;
}

#[tokio::test]
async fn empty_and_positive_wait_return_completed_output_and_nonzero_exit() {
    for wait in [json!({}), json!({"timeout_secs": 5})] {
        for exit_code in [0, 7] {
            let directory = TestDir::new();
            let server = server_with_paths(vec![directory.path().to_path_buf()]);
            let params = serde_json::from_value(json!({
                "command": format!("echo completed; echo diagnostic >&2; exit {exit_code}"),
                "working_dir": directory.path(),
                "wait": wait
            }))
            .unwrap();
            // A default wait resolves to 120 seconds, but a fast command must return promptly.
            let result = tokio::time::timeout(Duration::from_secs(5), server.spawn_impl(params))
                .await
                .unwrap()
                .unwrap();
            let text = text_content(&result);
            let output = OutputText(&text);
            assert_completed_exit(&output, exit_code);
            assert_completed_stream_output(&output);
            assert_execution_metadata_fields(&text);
            assert!(!text.contains("running_for:"));
            assert!(server.inner.spawned.lock().await.is_empty());
        }
    }
}

#[tokio::test]
async fn zero_wait_polls_immediately_and_process_can_be_waited_again() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let command = "while [ ! -f release ]; do sleep 0.01; done; echo finished";
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        server.spawn_impl(spawn_with_wait(command, directory.path(), wait_options(0))),
    )
    .await
    .expect("zero is an immediate poll, not an unlimited wait")
    .unwrap();
    let text = text_content(&result);
    let output = OutputText(&text);
    let execution_id = extract_field(&text, "execution_id");
    assert_running_wait_response(&output);
    let paths = assert_spawn_logs_exist(&output);
    assert!(server.inner.spawned.lock().await[&execution_id]
        .child
        .id()
        .is_some());

    std::fs::write(directory.path().join("release"), "").unwrap();
    let completed = text_content(&finish(&server, &execution_id).await);
    let completed_output = OutputText(&completed);
    assert_completed_wait_status(&completed_output);
    assert_eq!(extract_field(&completed, "execution_id"), execution_id);
    assert_completed_wait_paths(&completed_output, &paths);
    assert!(stream_block(&completed_output, OutputStream::Stdout).contains("finished"));
    assert!(server.inner.spawned.lock().await.is_empty());
}

#[tokio::test]
async fn running_and_completed_wait_share_grep_and_line_truncation() {
    let output = "printf 'keep-out-1\\ndrop-out\\nkeep-out-2\\nkeep-out-3\\nkeep-out-4\\n'; \
                  printf 'keep-err-1\\ndrop-err\\nkeep-err-2\\nkeep-err-3\\nkeep-err-4\\n' >&2";
    for running in [false, true] {
        let directory = TestDir::new();
        let server = server_with_paths(vec![directory.path().to_path_buf()]);
        let command = if running {
            format!("{output}; while [ ! -f release ]; do sleep 0.01; done")
        } else {
            output.to_string()
        };
        let options = WaitOptions {
            timeout_secs: Some(if running { 1 } else { 5 }),
            head_lines: Some(1),
            tail_lines: Some(1),
            max_output_bytes: Some(10_000),
            grep: Some("^keep".to_string()),
        };
        let result = server
            .spawn_impl(spawn_with_wait(&command, directory.path(), options))
            .await
            .unwrap();
        let text = text_content(&result);
        let output_text = OutputText(&text);
        let execution_id = extract_field(&text, "execution_id");
        assert_eq!(
            extract_field(&text, "status"),
            if running { "running" } else { "exited" }
        );
        assert_eq!(extract_field(&text, "total_lines"), "8");
        assert_execution_metadata_fields(&text);
        for stream in OUTPUT_STREAMS {
            assert_filtered_line_stream(&output_text, stream);
        }
        if running {
            let initial_runtime = extract_field(&text, "running_for");
            assert_running_process_can_finish(&server, &directory, &execution_id, &initial_runtime)
                .await;
        }
    }
}

async fn assert_byte_truncation_case(running: bool) {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let mut command =
        "echo keep-abcdefghijklmnopqrstuvwxyz; echo keep-ABCDEFGHIJKLMNOPQRSTUVWXYZ >&2"
            .to_string();
    if running {
        command.push_str("; while [ ! -f release ]; do sleep 0.01; done");
    }
    let result = server
        .spawn_impl(spawn_with_wait(
            &command,
            directory.path(),
            WaitOptions {
                timeout_secs: Some(if running { 1 } else { 5 }),
                max_output_bytes: Some(12),
                grep: Some("^keep".to_string()),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    let text = text_content(&result);
    let output_text = OutputText(&text);
    assert_byte_truncated_streams(&output_text);
    if running {
        terminate_and_reap(&server, &extract_field(&text, "execution_id")).await;
    }
}

#[tokio::test]
async fn running_and_completed_wait_share_byte_truncation() {
    for running in [false, true] {
        assert_byte_truncation_case(running).await;
    }
}

#[tokio::test]
async fn invalid_spawn_grep_fails_before_side_effects_or_log_creation() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let error = server
        .spawn_impl(spawn_with_wait(
            "touch must-not-exist",
            directory.path(),
            WaitOptions {
                grep: Some("[".to_string()),
                ..Default::default()
            },
        ))
        .await
        .unwrap_err();
    assert!(error.message.starts_with("invalid grep pattern:"));
    assert_invalid_spawn_side_effects(&directory, &server).await;
}

#[tokio::test]
async fn invalid_wait_grep_keeps_original_process_tracked() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let result = server
        .spawn_impl(spawn_params(
            "while [ ! -f release ]; do sleep 0.01; done; echo retained",
            directory.path(),
        ))
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&result), "execution_id");
    let child_id = server.inner.spawned.lock().await[&execution_id].child.id();
    let error = server
        .wait_impl(WaitParams {
            execution_id: execution_id.clone(),
            options: WaitOptions {
                grep: Some("[".to_string()),
                ..wait_options(0)
            },
        })
        .await
        .unwrap_err();
    assert!(error.message.starts_with("invalid grep pattern:"));
    assert_eq!(
        server.inner.spawned.lock().await[&execution_id].child.id(),
        child_id
    );
    std::fs::write(directory.path().join("release"), "").unwrap();
    let completed = text_content(&finish(&server, &execution_id).await);
    assert_eq!(extract_field(&completed, "status"), "exited");
    assert!(stream_block(&OutputText(&completed), OutputStream::Stdout).contains("retained"));
}

#[derive(Debug)]
struct FailOnceWait {
    child: Box<dyn ChildWrapper>,
    fail_next: bool,
}

impl ChildWrapper for FailOnceWait {
    fn inner(&self) -> &dyn ChildWrapper {
        self.child.as_ref()
    }
    fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
        self.child.as_mut()
    }
    fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
        self.child
    }
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = io::Result<ExitStatus>> + Send + '_>> {
        if std::mem::take(&mut self.fail_next) {
            Box::pin(async { Err(io::Error::other("injected wait failure")) })
        } else {
            self.child.wait()
        }
    }
}

#[tokio::test]
async fn wait_error_keeps_process_terminable_and_waitable() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let result = server
        .spawn_impl(spawn_params("sleep 30", directory.path()))
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&result), "execution_id");
    let mut entry = server
        .inner
        .spawned
        .lock()
        .await
        .remove(&execution_id)
        .unwrap();
    let child_id = entry.child.id();
    let started_at = entry.started_at;
    entry.child = Box::new(FailOnceWait {
        child: entry.child,
        fail_next: true,
    });
    server
        .store_spawned_process(execution_id.clone(), entry)
        .await;
    let error = server
        .wait_impl(WaitParams {
            execution_id: execution_id.clone(),
            options: wait_options(5),
        })
        .await
        .unwrap_err();
    assert!(error.message.contains("injected wait failure"));
    assert_eq!(
        server.inner.spawned.lock().await[&execution_id].child.id(),
        child_id
    );
    assert_eq!(
        server.inner.spawned.lock().await[&execution_id].started_at,
        started_at
    );
    terminate_and_reap(&server, &execution_id).await;
}

#[tokio::test]
async fn spawn_wait_preserves_snapshot_diffs_on_completion_and_after_expiry() {
    for expire_first in [false, true] {
        let directory = TestDir::new();
        gix::init(directory.path()).unwrap();
        std::fs::write(directory.path().join("note.txt"), "before\n").unwrap();
        let server = server_with_paths(vec![directory.path().to_path_buf()]);
        let command = if expire_first {
            "while [ ! -f release ]; do sleep 0.01; done; printf 'after\\n' > note.txt"
        } else {
            "printf 'after\\n' > note.txt"
        };
        let result = server
            .spawn_impl(spawn_with_wait(
                command,
                directory.path(),
                wait_options(if expire_first { 0 } else { 5 }),
            ))
            .await
            .unwrap();
        let result = if expire_first {
            let text = text_content(&result);
            assert_eq!(extract_field(&text, "status"), "running");
            assert!(!text.contains("```diff"));
            let execution_id = extract_field(&text, "execution_id");
            assert!(!server.inner.spawned.lock().await[&execution_id]
                .before_snap_ids
                .is_empty());
            std::fs::write(directory.path().join("release"), "").unwrap();
            finish(&server, &execution_id).await
        } else {
            result
        };
        let text = text_content(&result);
        assert_completed_wait_status(&OutputText(&text));
        let diff = snapshot_diff(&result);
        assert_diff_commit_and_path(diff);
        assert_diff_content(diff);
        assert_diff_closed_fence(diff);
    }
}
