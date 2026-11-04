use super::*;
use crate::fs_attachments::{AttachmentFetcher, AttachmentFs};

const CID: &str = "cid:media:pantheon%2Fatlas/python/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct FixtureFetcher(Semaphore);

#[async_trait]
impl AttachmentFetcher for FixtureFetcher {
    async fn fetch(&self, cid: &str, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        assert_eq!(cid, CID);
        let body = b"attachment\n";
        assert!(body.len() as u64 <= max_bytes);
        self.0.add_permits(1);
        Ok(body.to_vec())
    }

    async fn size(&self, _cid: &str) -> std::io::Result<Option<u64>> {
        Ok(Some(b"attachment\n".len() as u64))
    }
}

#[track_caller]
fn assert_status(result: &ScriptRun, expected_status: ScriptStatus) {
    assert_eq!(result.status, expected_status, "{result:?}");
}

#[track_caller]
fn assert_status_and_exit(
    result: &ScriptRun,
    expected_status: ScriptStatus,
    expected_exit_code: i32,
) {
    assert_eq!(result.status, expected_status, "{result:?}");
    assert_eq!(result.exit_code, expected_exit_code, "{result:?}");
}

#[track_caller]
fn assert_output_and_stderr_contains(
    result: &ScriptRun,
    expected_stdout: &str,
    stderr_needles: &[&str],
) {
    assert_eq!(result.stdout, expected_stdout, "{result:?}");
    for needle in stderr_needles {
        assert!(result.stderr.contains(needle), "{result:?}");
    }
}

#[track_caller]
fn assert_limit_hit_and_output(result: &ScriptRun, expected_stdout: &str) {
    assert_eq!(result.stdout, expected_stdout, "{result:?}");
    assert!(result.limit_hit.is_some(), "{result:?}");
    assert_eq!(result.error, result.limit_hit, "{result:?}");
    assert!(!result.stderr.is_empty(), "{result:?}");
}

#[track_caller]
fn assert_probe_joined_and_cancelled(command: &ProbeCommand) {
    assert_eq!(
        command.active.load(Ordering::SeqCst),
        0,
        "worker was not joined"
    );
    assert!(command.tokens.lock().unwrap()[0].is_cancelled());
}

#[track_caller]
fn assert_bounded_print_output(
    result: &ScriptRun,
    expected_error: &str,
    handled_marker: &str,
    max_memory_bytes: usize,
) {
    assert!(result.stderr.contains(expected_error), "{result:?}");
    assert!(result.stdout.ends_with(handled_marker), "{result:?}");
    assert!(result.stdout.len() <= max_memory_bytes + handled_marker.len());
}

#[track_caller]
fn assert_truncated_empty_stderr(result: &ScriptRun, expect_limit_hit: bool) {
    assert!(result.stderr.is_empty(), "{result:?}");
    assert!(result.stderr_truncated, "{result:?}");
    if expect_limit_hit {
        assert!(result.limit_hit.is_some(), "{result:?}");
    }
}

#[tokio::test]
async fn python_sum_alias_and_builder_only_opt_in() {
    let before = std::env::var_os("BASHKIT_ALLOW_INPROCESS_PYTHON");
    for command in ["python3", "python"] {
        assert!(BashKitBackend.builtin_names().iter().any(|n| n == command));
        let result = run(&format!("{command} -c 'print(sum(range(10)))'")).await;
        assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
        assert_eq!(result.stdout, "45\n");
        assert!(result.stderr.is_empty(), "{result:?}");
    }
    assert_eq!(std::env::var_os("BASHKIT_ALLOW_INPROCESS_PYTHON"), before);
    // A separate embedder did not inherit harnx's opt-in.
    let result = bashkit::Bash::builder()
        .python()
        .build()
        .exec("python3 -c 'print(45)'")
        .await
        .unwrap();
    assert_ne!(result.exit_code, 0);
    assert!(result.stderr.text_lossy().contains("disabled by default"));
}

#[tokio::test]
async fn python_heredoc_reads_tmp_and_lazy_full_cid_attachment() {
    let fetcher = Arc::new(FixtureFetcher(Semaphore::new(0)));
    let script = format!(
        "echo temporary > /tmp/input.txt\npython3 - <<'EOF'\nfrom pathlib import Path\nprint(open('/tmp/input.txt').read(), end='')\nprint(Path('/attachments/{CID}').read_text(), end='')\nPath('/tmp/from-python').write_text('written')\nEOF\ncat /tmp/from-python"
    );
    let result = BashKitBackend
        .run(crate::ScriptRequest {
            script: &script,
            limits: ScriptLimits::default(),
            commands: vec![],
            fs_mounts: vec![AttachmentFs::new(fetcher.clone()).mount()],
            cancel: CancellationToken::new(),
        })
        .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "temporary\nattachment\nwritten");
    assert_eq!(fetcher.0.available_permits(), 1);
}

#[tokio::test]
async fn python_attachment_open_is_binary_safe_and_writes_are_denied() {
    let fetcher = Arc::new(FixtureFetcher(Semaphore::new(0)));
    let script = format!(
        "python3 -c \"print(open('/attachments/{CID}', 'rb').read())\"; python3 -c \"open('/attachments/{CID}', 'w')\""
    );
    let result = BashKitBackend
        .run(crate::ScriptRequest {
            script: &script,
            limits: ScriptLimits::default(),
            commands: vec![],
            fs_mounts: vec![AttachmentFs::new(fetcher.clone()).mount()],
            cancel: CancellationToken::new(),
        })
        .await;
    assert_status(&result, ScriptStatus::Error);
    assert_output_and_stderr_contains(&result, "b'attachment\\n'\n", &["read-only"]);
    assert_eq!(fetcher.0.available_permits(), 1);
}

#[tokio::test]
async fn python_file_and_stdin_forms_share_vfs() {
    let result =
        run("echo \"print(45)\" > /tmp/main.py\npython /tmp/main.py\nprintf 'print(45)' | python3")
            .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "45\n45\n");
}

#[tokio::test]
async fn python_cannot_call_registered_tools_or_host_services() {
    let command = ProbeCommand::new(Behavior::Pending);
    for code in [
        "probe()",
        "call_tool('probe', {})",
        "tools.probe()",
        "import subprocess; subprocess.run(['probe'])",
        "import os; os.system('probe')",
        "import socket",
        "import re",
    ] {
        let result = BashKitBackend
            .run(crate::ScriptRequest {
                script: &format!("python3 - <<'EOF'\n{code}\nEOF"),
                limits: ScriptLimits::default(),
                commands: vec![command.clone()],
                fs_mounts: vec![],
                cancel: CancellationToken::new(),
            })
            .await;
        assert_eq!(result.status, ScriptStatus::Error, "{code}: {result:?}");
        assert!(!result.stderr.is_empty(), "{code}: {result:?}");
    }
    assert!(
        command.tokens.lock().unwrap().is_empty(),
        "Python dispatched a tool"
    );
    assert_eq!(command.active.load(Ordering::SeqCst), 0);
    let host = std::env::current_exe().unwrap();
    let result = run(&format!(
        "python3 - <<'EOF'\nprint(open({:?}, 'rb').read())\nEOF",
        host.to_string_lossy()
    ))
    .await;
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert!(result.stderr.contains("FileNotFoundError"), "{result:?}");
}

#[test]
fn python_limits_come_from_run_limits() {
    let limits = ScriptLimits {
        timeout_secs: 9,
        max_python_memory_bytes: 32 * 1024,
        ..Default::default()
    };
    let limits = super::super::python_limits(&limits);
    assert_eq!(limits.common.max_duration, Duration::from_secs(9));
    assert_eq!(limits.common.max_memory, 32 * 1024);
    for timeout_secs in [0, u64::MAX] {
        assert_eq!(
            super::super::python_limits(&ScriptLimits {
                timeout_secs,
                ..Default::default()
            })
            .common
            .max_duration,
            NO_DEADLINE
        );
    }
}

#[tokio::test]
async fn python_memory_limit_reports_clear_stderr_and_preserves_prior_output() {
    let result = BashKitBackend
        .run(crate::ScriptRequest {
            script: "echo before; python3 -c 'x = [0] * 1000000'; echo after",
            limits: ScriptLimits {
                max_python_memory_bytes: 32 * 1024,
                ..Default::default()
            },
            commands: vec![],
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        })
        .await;
    assert_status_and_exit(&result, ScriptStatus::Error, 1);
    assert_output_and_stderr_contains(&result, "before\n", &["MemoryError", "32768"]);
    assert_eq!(run("python3 -c 'print(45)'").await.stdout, "45\n");
}

#[tokio::test]
async fn python_time_limit_stops_busy_loop_with_limit_hit() {
    let start = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        BashKitBackend.run(crate::ScriptRequest {
            script: "echo before; sleep 0.2; python3 -c 'while True: pass'; echo after",
            limits: ScriptLimits {
                timeout_secs: 1,
                ..Default::default()
            },
            commands: vec![],
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        }),
    )
    .await
    .expect("Python busy loop hung");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "timeout was refreshed for Python"
    );
    assert_status_and_exit(&result, ScriptStatus::TimedOut, 124);
    assert_limit_hit_and_output(&result, "before\n");
}

// Current-thread tests ensure async cancellation is not relying on a spare
// executor worker while Monty occupies a synchronous VM slice.
async fn busy_python() -> (
    tokio::task::JoinHandle<ScriptRun>,
    CancellationToken,
    Arc<ProbeCommand>,
) {
    let command = ProbeCommand::new(Behavior::Pending);
    let fetcher = Arc::new(FixtureFetcher(Semaphore::new(0)));
    let token = CancellationToken::new();
    let task_command = command.clone();
    let task_fetcher = fetcher.clone();
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        let script = format!(
            "probe & python3 - <<'EOF'\nfirst = True\nwhile True:\n if first:\n  first = False\n  open('/attachments/{CID}').read()\nEOF\nwait"
        );
        BashKitBackend
            .run(crate::ScriptRequest {
                script: &script,
                limits: ScriptLimits {
                    timeout_secs: 10,
                    ..Default::default()
                },
                commands: vec![task_command],
                fs_mounts: vec![AttachmentFs::new(task_fetcher).mount()],
                cancel: task_token,
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), fetcher.0.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    tokio::time::timeout(Duration::from_secs(2), command.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    (task, token, command)
}

#[tokio::test]
async fn python_cancel_busy_loop_within_one_second_and_join_worker() {
    let (task, token, command) = busy_python().await;
    let start = Instant::now();
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("Python cancellation hung")
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_status_and_exit(&result, ScriptStatus::Cancelled, 130);
    assert_probe_joined_and_cancelled(&command);
}

#[tokio::test]
async fn python_caller_drop_stops_busy_loop_and_drains_jobs() {
    let (task, token, command) = busy_python().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while command.active.load(Ordering::SeqCst) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Python worker survived caller drop");
    assert!(command.tokens.lock().unwrap()[0].is_cancelled());
    assert!(!token.is_cancelled());
}

#[tokio::test]
async fn python_supports_simple_classes_but_not_inheritance() {
    let result = run("python3 - <<'EOF'\nclass Tool:\n    def value(self):\n        return 45\nprint(Tool().value())\nEOF").await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "45\n");
    let result = run("python3 - <<'EOF'\nclass Base: pass\nclass Child(Base): pass\nEOF").await;
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert!(!result.stderr.is_empty());
}

#[tokio::test]
async fn python_print_buffer_is_bounded_and_memory_error_can_be_handled() {
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        BashKitBackend.run(crate::ScriptRequest {
            script: "set +e; python3 -c \"while True: print('x' * 1000)\"; echo handled",
            limits: ScriptLimits {
                max_python_memory_bytes: 32 * 1024,
                ..Default::default()
            },
            commands: vec![],
            fs_mounts: vec![],
            cancel: CancellationToken::new(),
        }),
    )
    .await
    .expect("print collector did not enforce memory ceiling");
    assert_status(&result, ScriptStatus::Ok);
    assert_bounded_print_output(
        &result,
        "MemoryError: memory limit exceeded",
        "handled\n",
        32 * 1024,
    );
}

#[test]
fn python_queued_worker_cancel_and_deadline_do_not_wait_for_blocking_pool() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let (started, ready) = std::sync::mpsc::channel();
    let blocker = runtime.spawn_blocking(move || {
        started.send(()).unwrap();
        let _ = wait.recv();
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    runtime.block_on(async {
        for timed_out in [false, true] {
            let token = CancellationToken::new();
            let task_token = token.clone();
            let task = tokio::spawn(async move {
                BashKitBackend
                    .run(crate::ScriptRequest {
                        script: "python3 -c 'while True: pass'",
                        limits: ScriptLimits {
                            timeout_secs: 1,
                            max_stderr_bytes: 0,
                            ..Default::default()
                        },
                        commands: vec![],
                        fs_mounts: vec![],
                        cancel: task_token,
                    })
                    .await
            });
            tokio::task::yield_now().await;
            if !timed_out {
                token.cancel();
            }
            let result = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("cancelled script waited for pool capacity")
                .unwrap();
            let (status, exit_code) = if timed_out {
                (ScriptStatus::TimedOut, 124)
            } else {
                (ScriptStatus::Cancelled, 130)
            };
            assert_status_and_exit(&result, status, exit_code);
            assert_truncated_empty_stderr(&result, timed_out);
            if timed_out {
                assert_eq!(result.error, result.limit_hit);
            } else {
                assert_eq!(result.error.as_deref(), Some("cancelled"));
            }
        }
    });
    drop(release);
    runtime.block_on(blocker).unwrap();
}
