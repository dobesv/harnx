use super::*;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::watch;

fn spawn_with_wait(command: &str, directory: &Path, options: WaitOptions) -> SpawnCommandParams {
    SpawnCommandParams {
        wait: Some(options),
        ..spawn_params(command, directory)
    }
}

pub(super) fn wait_options(seconds: u64) -> WaitOptions {
    WaitOptions {
        seconds: Some(seconds),
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
    let mut spawned = server.inner.spawned.lock().await;
    let entry = Arc::get_mut(spawned.get_mut(execution_id).unwrap()).unwrap();
    entry.started_at = started_at - Duration::from_secs(10);
    drop(spawned);

    let poll = text_content(
        &server
            .status_impl(status_params(execution_id))
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
        .wait_impl(wait_params_with(execution_id.to_string(), wait_options(5)))
        .await
        .unwrap()
}

pub(super) async fn terminate_and_reap(server: &BashServer, execution_id: &str) {
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
    assert!(tracked_exit(server, execution_id).await.is_some());
}

/// The exit the supervisor recorded for a still-tracked process.
async fn tracked_exit(server: &BashServer, execution_id: &str) -> Option<ProcessExit> {
    server
        .get_spawned_entry(execution_id)
        .await
        .unwrap()
        .exit
        .borrow()
        .clone()
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
    for wait in [json!({}), json!({"seconds": 5})] {
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
            let execution_id = extract_field(&text, "execution_id");
            assert!(tracked_exit(&server, &execution_id).await.is_some());
        }
    }
}

#[tokio::test]
async fn status_polls_immediately_and_process_can_be_waited_again() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let command = "while [ ! -f release ]; do sleep 0.01; done; echo finished";
    let spawned = server
        .spawn_impl(spawn_params(command, directory.path()))
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&spawned), "execution_id");
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        server.status_impl(status_params(&execution_id)),
    )
    .await
    .expect("status never waits")
    .unwrap();
    let text = text_content(&result);
    let output = OutputText(&text);
    assert_running_wait_response(&output);
    let paths = assert_spawn_logs_exist(&output);
    assert_eq!(tracked_exit(&server, &execution_id).await, None);

    std::fs::write(directory.path().join("release"), "").unwrap();
    let completed = text_content(&finish(&server, &execution_id).await);
    let completed_output = OutputText(&completed);
    assert_completed_wait_status(&completed_output);
    assert_eq!(extract_field(&completed, "execution_id"), execution_id);
    assert_completed_wait_paths(&completed_output, &paths);
    assert!(stream_block(&completed_output, OutputStream::Stdout).contains("finished"));
    assert!(tracked_exit(&server, &execution_id).await.is_some());
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
            forever: None,
            seconds: Some(if running { 1 } else { 5 }),
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
                seconds: Some(if running { 1 } else { 5 }),
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
    let tracked = server.get_spawned_entry(&execution_id).await.unwrap();
    let error = server
        .wait_impl(wait_params_with(
            execution_id.clone(),
            WaitOptions {
                grep: Some("[".to_string()),
                ..wait_options(1)
            },
        ))
        .await
        .unwrap_err();
    assert!(error.message.starts_with("invalid grep pattern:"));
    let still_tracked = server.get_spawned_entry(&execution_id).await.unwrap();
    assert!(Arc::ptr_eq(&tracked, &still_tracked));
    std::fs::write(directory.path().join("release"), "").unwrap();
    let completed = text_content(&finish(&server, &execution_id).await);
    assert_eq!(extract_field(&completed, "status"), "exited");
    assert!(stream_block(&OutputText(&completed), OutputStream::Stdout).contains("retained"));
}

/// A child whose next `failures_left` waits fail without touching the process.
#[derive(Debug)]
struct FailingWait {
    child: Box<dyn ChildWrapper>,
    failures_left: usize,
    attempts: Arc<AtomicUsize>,
}

impl FailingWait {
    fn new(child: Box<dyn ChildWrapper>, failures_left: usize) -> Box<Self> {
        Box::new(Self {
            child,
            failures_left,
            attempts: Arc::default(),
        })
    }
}

impl ChildWrapper for FailingWait {
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
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.failures_left > 0 {
            self.failures_left -= 1;
            Box::pin(async { Err(io::Error::other("injected wait failure")) })
        } else {
            self.child.wait()
        }
    }
}

#[tokio::test]
async fn wait_error_keeps_process_supervised() {
    let child = FailingWait::new(spawn_group_leader("sleep 30"), 1);
    let (mut exit, control) = spawn_supervisor(child, Duration::from_secs(30));
    // A failed wait proves nothing about the process, so no exit is recorded.
    let early = tokio::time::timeout(Duration::from_millis(300), exit.wait_for(Option::is_some));
    assert!(
        early.await.is_err(),
        "a failed wait must not record an exit"
    );

    control.send(ProcessControl::Kill).unwrap();
    let recorded = recorded_exit(&mut exit).await;
    assert!(!recorded.timed_out, "{recorded:?}");
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
        let wait = if expire_first {
            None
        } else {
            Some(wait_options(5))
        };
        let result = server
            .spawn_impl(SpawnCommandParams {
                wait,
                ..spawn_params(command, directory.path())
            })
            .await
            .unwrap();
        let result = if expire_first {
            let text = text_content(&result);
            let execution_id = extract_field(&text, "execution_id");
            let text = text_content(
                &server
                    .status_impl(status_params(&execution_id))
                    .await
                    .unwrap(),
            );
            assert_eq!(extract_field(&text, "status"), "running");
            assert!(!text.contains("```diff"));
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

fn status_params(execution_id: &str) -> StatusParams {
    StatusParams {
        execution_id: execution_id.to_owned(),
        head_lines: None,
        tail_lines: None,
        max_output_bytes: None,
        grep: None,
    }
}

/// A `wait` call carrying `options`, as the dispatcher would parse it.
pub(super) fn wait_params_with(
    execution_id: impl Into<String>,
    options: WaitOptions,
) -> WaitParams {
    WaitParams {
        execution_id: execution_id.into(),
        seconds: options.seconds,
        forever: options.forever,
        head_lines: options.head_lines,
        tail_lines: options.tail_lines,
        max_output_bytes: options.max_output_bytes,
        grep: options.grep,
    }
}

pub(super) fn wait_params(execution_id: &str, timeout_secs: u64) -> WaitParams {
    wait_params_with(execution_id.to_owned(), wait_options(timeout_secs))
}

async fn spawn_execution(server: &BashServer, command: &str, directory: &Path) -> String {
    let result = server
        .spawn_impl(spawn_params(command, directory))
        .await
        .unwrap();
    extract_field(&text_content(&result), "execution_id")
}

async fn send_sigterm(
    server: &BashServer,
    execution_id: &str,
) -> Result<CallToolResult, ErrorData> {
    server
        .terminate_impl(TerminateParams {
            execution_id: execution_id.to_owned(),
            signal: Some("SIGTERM".to_owned()),
        })
        .await
}

/// Counts the history snapshots labelled `label` in `directory`'s repo. A
/// fresh probe snapshot's parent chain holds every snapshot taken before it.
async fn count_snapshots(server: &BashServer, directory: &Path, label: &str) -> usize {
    let directory = directory.canonicalize().unwrap();
    let probe = server
        .inner
        .history
        .snapshot_repos_for_dir(&directory, "probe")
        .await
        .unwrap();
    let repo = gix::open(&directory).unwrap();
    let mut next = probe.first().map(|(_, id)| *id);
    let mut count = 0;
    while let Some(id) = next {
        let commit = repo.find_commit(id).unwrap();
        count += usize::from(commit.message_raw_sloppy() == label);
        next = commit.parent_ids().next().map(|parent| parent.detach());
    }
    count
}

#[tokio::test]
async fn cancelled_wait_keeps_process_reachable() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let execution_id = spawn_execution(&server, "sleep 30", directory.path()).await;
    let dropped = tokio::time::timeout(
        Duration::from_millis(200),
        server.wait_impl(wait_params(&execution_id, 30)),
    )
    .await;
    assert!(
        dropped.is_err(),
        "the wait must still be pending when dropped"
    );

    let poll = text_content(
        &server
            .wait_impl(wait_params(&execution_id, 1))
            .await
            .unwrap(),
    );
    assert_eq!(extract_field(&poll, "status"), "running");
    terminate_and_reap(&server, &execution_id).await;
}

#[tokio::test]
async fn terminate_during_wait_ends_wait() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let execution_id = spawn_execution(&server, "sleep 30", directory.path()).await;
    let started = std::time::Instant::now();
    let waiter = tokio::spawn({
        let server = server.clone();
        let params = wait_params(&execution_id, 30);
        async move { server.wait_impl(params).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    send_sigterm(&server, &execution_id).await.unwrap();
    let waited = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("terminating the process must end the wait")
        .unwrap()
        .unwrap();
    assert_completed_wait_status(&OutputText(&text_content(&waited)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn concurrent_waits_both_see_exit() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let execution_id = spawn_execution(&server, "sleep 1", directory.path()).await;
    let (first, second) = tokio::join!(
        server.wait_impl(wait_params(&execution_id, 5)),
        server.wait_impl(wait_params(&execution_id, 5)),
    );
    for result in [first, second] {
        assert_completed_exit(&OutputText(&text_content(&result.unwrap())), 0);
    }
}

#[tokio::test]
async fn repeated_wait_after_exit_reuses_snapshot() {
    let directory = TestDir::new();
    gix::init(directory.path()).unwrap();
    std::fs::write(directory.path().join("note.txt"), "before\n").unwrap();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let command = "printf 'after\\n' > note.txt";
    let execution_id = spawn_execution(&server, command, directory.path()).await;

    let first = finish(&server, &execution_id).await;
    let second = finish(&server, &execution_id).await;
    assert_eq!(snapshot_diff(&first), snapshot_diff(&second));
    assert_eq!(
        count_snapshots(&server, directory.path(), "after wait").await,
        1
    );
}

#[tokio::test]
async fn status_after_exit_reuses_the_wait_snapshot() {
    let directory = TestDir::new();
    gix::init(directory.path()).unwrap();
    std::fs::write(directory.path().join("note.txt"), "before\n").unwrap();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let execution_id =
        spawn_execution(&server, "printf 'after\\n' > note.txt", directory.path()).await;

    let waited = finish(&server, &execution_id).await;
    let first = server
        .status_impl(status_params(&execution_id))
        .await
        .unwrap();
    let second = server
        .status_impl(status_params(&execution_id))
        .await
        .unwrap();
    assert_diff_content(snapshot_diff(&waited));
    assert_eq!(snapshot_diff(&waited), snapshot_diff(&first));
    assert_eq!(snapshot_diff(&first), snapshot_diff(&second));
    assert_eq!(
        count_snapshots(&server, directory.path(), "after wait").await,
        1
    );
}

fn spawn_group_leader(script: &str) -> Box<dyn ChildWrapper> {
    let mut command = CommandWrap::with_new("sh", |command| {
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    });
    // Kill whatever a test leaves behind once the supervisor drops the child.
    command.wrap(KillOnDrop).wrap(ProcessGroup::leader());
    command.spawn().unwrap()
}

async fn recorded_exit(exit: &mut watch::Receiver<Option<ProcessExit>>) -> ProcessExit {
    tokio::time::timeout(Duration::from_secs(10), exit.wait_for(Option::is_some))
        .await
        .expect("the supervisor must record an exit")
        .unwrap()
        .clone()
        .expect("wait_for only returns a recorded exit")
}

/// A zombie has already exited; only its new parent's reap is outstanding.
fn process_is_gone(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        return stat
            .rsplit_once(')')
            .is_some_and(|(_, fields)| fields.trim_start().starts_with('Z'));
    }
    // SAFETY: signal 0 performs only the existence and permission checks.
    let result = unsafe { libc::kill(pid, 0) };
    result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

async fn wait_until_gone(pid: i32) -> bool {
    for _ in 0..100 {
        if process_is_gone(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn supervisor_deadline_kills_process_group() {
    let directory = TestDir::new();
    let pid_file = directory.path().join("background.pid");
    let script = format!("sleep 30 & echo $! > '{}'; sleep 30", pid_file.display());
    let (mut exit, _control) =
        spawn_supervisor(spawn_group_leader(&script), Duration::from_secs(1));

    assert!(recorded_exit(&mut exit).await.timed_out);
    let background: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_until_gone(background).await,
        "background sleep {background} outlived the deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn supervisor_reports_persistent_wait_error() {
    let child = FailingWait::new(spawn_group_leader("sleep 30"), usize::MAX);
    let attempts = Arc::clone(&child.attempts);
    let (mut exit, _control) = spawn_supervisor(child, Duration::from_secs(3600));

    let recorded = recorded_exit(&mut exit).await;
    assert_eq!(recorded.code, None, "{recorded:?}");
    assert_eq!(
        recorded.wait_error.as_deref(),
        Some("injected wait failure")
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 5);
}

#[tokio::test(start_paused = true)]
async fn wait_reports_supervisor_wait_error() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let metadata = SpawnMetadata {
        command: "sleep 30".to_owned(),
        working_dir: directory.path().to_path_buf(),
        stdout_log_path: directory.path().join("stdout.log"),
        stderr_log_path: directory.path().join("stderr.log"),
        before_snap_ids: Vec::new(),
        snapshot_decision: SnapshotDecision::ReadOnly,
    };
    let child = FailingWait::new(spawn_group_leader("sleep 30"), usize::MAX);
    let entry = SpawnedProcess::start(child, Duration::from_secs(3600), metadata);
    server
        .store_spawned_process("exec-failing".to_owned(), entry)
        .await;

    let expected = "failed waiting for execution_id 'exec-failing': injected wait failure";
    // The error stays recorded, so repeated waits and terminate all report it.
    for _ in 0..2 {
        let error = server
            .wait_impl(wait_params("exec-failing", 30))
            .await
            .unwrap_err();
        assert_eq!(error.message, expected);
    }
    let error = send_sigterm(&server, "exec-failing").await.unwrap_err();
    assert_eq!(error.message, expected);
}

#[tokio::test]
async fn status_reports_running_then_exited() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let spawned = server
        .spawn_impl(spawn_params(
            "echo started; while [ ! -f release ]; do sleep 0.01; done; exit 3",
            directory.path(),
        ))
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&spawned), "execution_id");

    let started = std::time::Instant::now();
    let running = text_content(
        &server
            .status_impl(status_params(&execution_id))
            .await
            .unwrap(),
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(extract_field(&running, "status"), "running");
    assert!(extract_field(&running, "running_for").ends_with('s'));

    std::fs::write(directory.path().join("release"), "").unwrap();
    finish(&server, &execution_id).await;
    let exited = text_content(
        &server
            .status_impl(status_params(&execution_id))
            .await
            .unwrap(),
    );
    assert_eq!(extract_field(&exited, "status"), "exited");
    assert_eq!(extract_field(&exited, "exit_code"), "3");
}

#[tokio::test]
async fn spawn_deadline_kills_the_process_and_status_reports_timed_out() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let spawned = server
        .spawn_impl(SpawnCommandParams {
            timeout_secs: Some(1),
            ..spawn_params("sleep 30", directory.path())
        })
        .await
        .unwrap();
    let execution_id = extract_field(&text_content(&spawned), "execution_id");

    let result = server
        .wait_impl(wait_params(&execution_id, 10))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let text = text_content(&result);
    assert_eq!(extract_field(&text, "status"), "timed_out");
    assert!(text.contains(&format!(
        "execution_id '{execution_id}' killed after its 1s deadline"
    )));
    let again = text_content(
        &server
            .status_impl(status_params(&execution_id))
            .await
            .unwrap(),
    );
    assert_eq!(extract_field(&again, "status"), "timed_out");
}

#[tokio::test]
async fn forever_wait_returns_when_the_process_exits() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    let result = server
        .spawn_impl(spawn_with_wait(
            "sleep 0.2; echo done",
            directory.path(),
            WaitOptions {
                forever: Some(true),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(extract_field(&text_content(&result), "status"), "exited");
}

#[tokio::test]
async fn invalid_spawn_deadline_and_wait_fail_without_side_effects() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    for timeout_secs in [0, 86_401] {
        let error = server
            .spawn_impl(SpawnCommandParams {
                timeout_secs: Some(timeout_secs),
                ..spawn_params("touch must-not-exist", directory.path())
            })
            .await
            .unwrap_err();
        assert!(error.message.contains("between 1 and 86400"), "{error:?}");
    }
    let error = server
        .spawn_impl(spawn_with_wait(
            "touch must-not-exist",
            directory.path(),
            wait_options(0),
        ))
        .await
        .unwrap_err();
    assert!(error.message.contains("between 1 and 240"), "{error:?}");
    assert_invalid_spawn_side_effects(&directory, &server).await;
}

#[tokio::test]
async fn exec_rejects_zero_and_oversized_deadlines() {
    let directory = TestDir::new();
    let server = server_with_paths(vec![directory.path().to_path_buf()]);
    for timeout_secs in [0, 86_401] {
        let error = server
            .exec_command_impl(ExecCommandParams {
                timeout_secs: Some(timeout_secs),
                ..exec_params("touch must-not-exist", directory.path())
            })
            .await
            .unwrap_err();
        assert!(error.message.contains("between 1 and 86400"), "{error:?}");
    }
    assert!(!directory.path().join("must-not-exist").exists());
}
