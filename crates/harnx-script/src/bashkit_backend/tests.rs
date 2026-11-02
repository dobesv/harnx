use std::{
    path::Path,
    sync::{atomic::AtomicUsize, atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tokio::sync::{Barrier, Semaphore};

use super::{execution_limits, BashKitBackend, NO_DEADLINE};
use crate::{
    CancellationToken, FileSystem, FsMount, InMemoryFs, ReadOnlyFs, ScriptBackend, ScriptCommand,
    ScriptCommandContext, ScriptCommandOutput, ScriptLimits, ScriptRun, ScriptStatus,
};

async fn run(script: &str) -> ScriptRun {
    BashKitBackend
        .run(
            script,
            ScriptLimits::default(),
            vec![],
            vec![],
            CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn echo_pipe_jq_rg_on_vfs_files() {
    let result = run(r#"echo '{"items":["forge","anvil"]}' > data.json
jq -r '.items[]' data.json | rg '^forge$' > selected
cat selected
pwd"#)
    .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "forge\n/tmp\n");
    assert_eq!(result.exit_code, 0);
}

#[tokio::test]
async fn mounts_are_binary_safe_and_can_be_read_only() {
    let fs = Arc::new(InMemoryFs::new());
    fs.write_file(Path::new("/blob"), &[0, 1, 2, 255])
        .await
        .unwrap();
    let result = BashKitBackend
        .run(
            "base64 -w0 /attachments/blob; echo; echo fail > /attachments/blob",
            ScriptLimits::default(),
            vec![],
            vec![FsMount {
                path: "/attachments".into(),
                fs: Arc::new(ReadOnlyFs::new(fs.clone())),
            }],
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.stdout, "AAEC/w==\n");
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert_eq!(
        fs.read_file(Path::new("/blob")).await.unwrap(),
        [0, 1, 2, 255]
    );
}

#[tokio::test]
async fn errexit_default_and_script_opt_out() {
    let failed = run("echo before; false; echo after").await;
    assert_eq!(failed.status, ScriptStatus::Error);
    assert_eq!(failed.stdout, "before\n");
    assert_eq!(failed.exit_code, 1);
    let opted_out = run("set +e; false; echo after").await;
    assert_eq!(opted_out.stdout, "after\n");
    assert_eq!(opted_out.status, ScriptStatus::Ok);
}

#[tokio::test]
async fn each_run_has_fresh_shell_files_and_cancel_flag() {
    let token = CancellationToken::new();
    token.cancel();
    let cancelled = BashKitBackend
        .run(
            "echo should-not-run",
            ScriptLimits::default(),
            vec![],
            vec![],
            token,
        )
        .await;
    assert_eq!(cancelled.status, ScriptStatus::Cancelled);
    assert!(cancelled.stdout.is_empty());
    assert_eq!(
        run("echo secret > file; export SECRET=secret")
            .await
            .exit_code,
        0
    );
    let next = run("echo \"${SECRET-unset}\"; test ! -e file").await;
    assert_eq!(next.stdout, "unset\n");
    assert_eq!(next.status, ScriptStatus::Ok);
}

#[tokio::test]
async fn host_files_network_and_python_are_unavailable() {
    let names = BashKitBackend.builtin_names();
    for name in ["curl", "wget", "python", "python3"] {
        assert!(!names.iter().any(|n| n == name), "unexpected {name}");
        let result = run(&format!("{name} https://127.0.0.1:1/")).await;
        assert_eq!(result.exit_code, 127, "{result:?}");
    }
    // `/etc/hostname` is a virtual rootfs file, not the host's hostname.
    assert_eq!(run("cat /etc/hostname").await.stdout, "bashkit-sandbox\n");
    let host_file = std::env::current_exe().unwrap();
    let quoted = host_file.to_string_lossy().replace('\'', "'\\''");
    let result = run(&format!("cat '{quoted}'")).await;
    assert_ne!(result.exit_code, 0, "{result:?}");
}

#[test]
fn wall_time_overrides_bashkit_default_and_zero_disables_it() {
    assert_eq!(
        execution_limits(&ScriptLimits::default()).timeout,
        Duration::from_secs(86_400)
    );
    let limits = ScriptLimits {
        timeout_secs: 0,
        ..Default::default()
    };
    assert_eq!(execution_limits(&limits).timeout, NO_DEADLINE);
    let limits = ScriptLimits {
        timeout_secs: u64::MAX,
        ..Default::default()
    };
    assert_eq!(execution_limits(&limits).timeout, NO_DEADLINE);
}

#[tokio::test]
async fn command_limit_is_reported_with_partial_output() {
    let result = BashKitBackend
        .run(
            "echo before\necho middle\necho after",
            ScriptLimits {
                max_commands: 3,
                ..Default::default()
            },
            vec![],
            vec![],
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
    assert!(result.limit_hit.is_some(), "{result:?}");
    assert!(result.stdout.starts_with("before\n"), "{result:?}");
    assert!(!result.stdout.contains("after"));
}

#[tokio::test]
async fn output_caps_report_truncation_and_keep_utf8_valid() {
    let result = BashKitBackend
        .run(
            "printf 'éééé'; printf 'abcdef' >&2",
            ScriptLimits {
                max_stdout_bytes: 5,
                max_stderr_bytes: 3,
                ..Default::default()
            },
            vec![],
            vec![],
            CancellationToken::new(),
        )
        .await;
    assert!(result.stdout.len() <= 5, "{result:?}");
    assert!(result.stderr.len() <= 3, "{result:?}");
    assert!(result.stdout_truncated, "{result:?}");
    assert!(result.stderr_truncated, "{result:?}");
    assert!(result.limit_hit.is_some());
}

#[tokio::test]
async fn syntax_error_reports_error() {
    let result = run("echo before\nif").await;
    assert_eq!(result.status, ScriptStatus::Error);
    assert_eq!(result.exit_code, 2, "{result:?}");
    assert!(result.stderr.contains("syntax error"), "{result:?}");
}

enum Behavior {
    Barrier(Barrier),
    Pending,
    Fatal,
}

struct ProbeCommand {
    behavior: Behavior,
    active: Arc<AtomicUsize>,
    started: Semaphore,
    tokens: Mutex<Vec<CancellationToken>>,
}

impl ProbeCommand {
    fn new(behavior: Behavior) -> Arc<Self> {
        Arc::new(Self {
            behavior,
            active: Arc::new(AtomicUsize::new(0)),
            started: Semaphore::new(0),
            tokens: Mutex::new(vec![]),
        })
    }
}

struct ActiveGuard(Arc<AtomicUsize>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ScriptCommand for ProbeCommand {
    fn name(&self) -> &str {
        "probe"
    }

    async fn run(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _guard = ActiveGuard(self.active.clone());
        self.tokens.lock().unwrap().push(ctx.cancel);
        self.started.add_permits(1);
        match &self.behavior {
            Behavior::Barrier(barrier) => {
                barrier.wait().await;
                // Reaching this point requires two simultaneous invocations.
                assert_eq!(ctx.cwd, Path::new("/tmp"));
                assert_eq!(args, ["--flag"]);
                assert!(stdin.is_none());
                Ok(ScriptCommandOutput {
                    stdout: "overlapped\n".into(),
                    ..Default::default()
                })
            }
            Behavior::Pending => std::future::pending().await,
            Behavior::Fatal => Err("fatal inner call".into()),
        }
    }
}

#[tokio::test]
async fn background_wait_registered_async_commands_overlap() {
    let command = ProbeCommand::new(Behavior::Barrier(Barrier::new(2)));
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        BashKitBackend.run(
            "probe --flag & probe --flag & wait",
            ScriptLimits::default(),
            vec![command.clone()],
            vec![],
            CancellationToken::new(),
        ),
    )
    .await
    .expect("commands ran sequentially");
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout.matches("overlapped").count(), 2);
    assert_eq!(command.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancel_mid_loop_stops_within_one_second_without_orphan_jobs() {
    let command = ProbeCommand::new(Behavior::Pending);
    let token = CancellationToken::new();
    let c = command.clone();
    let t = token.clone();
    let task = tokio::spawn(async move {
        BashKitBackend
            .run(
                "probe & probe & while true; do sleep 0.01; done",
                ScriptLimits::default(),
                vec![c],
                vec![],
                t,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), command.started.acquire_many(2))
        .await
        .unwrap()
        .unwrap()
        .forget();
    let start = Instant::now();
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(result.status, ScriptStatus::Cancelled, "{result:?}");
    assert_eq!(result.exit_code, 130);
    assert_eq!(
        command.active.load(Ordering::SeqCst),
        0,
        "orphan command future"
    );
    assert!(command
        .tokens
        .lock()
        .unwrap()
        .iter()
        .all(CancellationToken::is_cancelled));
}

#[tokio::test]
async fn cancel_interrupts_pending_foreground_command() {
    let command = ProbeCommand::new(Behavior::Pending);
    let token = CancellationToken::new();
    let c = command.clone();
    let t = token.clone();
    let task = tokio::spawn(async move {
        BashKitBackend
            .run(
                "echo before; probe",
                ScriptLimits::default(),
                vec![c],
                vec![],
                t,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), command.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, ScriptStatus::Cancelled, "{result:?}");
    assert_eq!(result.stdout, "before\n");
    assert_eq!(command.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn timeout_reports_limit_and_drops_background_jobs() {
    let command = ProbeCommand::new(Behavior::Pending);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        BashKitBackend.run(
            "echo before; probe & sleep 100; wait",
            ScriptLimits {
                timeout_secs: 1,
                ..Default::default()
            },
            vec![command.clone()],
            vec![],
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result.status, ScriptStatus::TimedOut, "{result:?}");
    assert_eq!(result.exit_code, 124);
    assert!(result.limit_hit.is_some());
    assert_eq!(result.stdout, "before\n");
    assert_eq!(command.active.load(Ordering::SeqCst), 0);
    assert_eq!(command.tokens.lock().unwrap().len(), 1);
    assert!(command.tokens.lock().unwrap()[0].is_cancelled());
}

#[tokio::test]
async fn dropping_run_cleans_up_pending_jobs_and_signals_command_work() {
    let command = ProbeCommand::new(Behavior::Pending);
    let c = command.clone();
    let external = CancellationToken::new();
    let token = external.clone();
    let task = tokio::spawn(async move {
        BashKitBackend
            .run(
                "probe & sleep 100",
                ScriptLimits::default(),
                vec![c],
                vec![],
                token,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), command.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(command.active.load(Ordering::SeqCst), 0);
    assert!(command.tokens.lock().unwrap()[0].is_cancelled());
    assert!(
        !external.is_cancelled(),
        "run must not cancel caller's token"
    );
}

#[tokio::test]
async fn fatal_registered_command_stops_even_with_errexit_disabled() {
    for script in [
        "set +e; probe; echo after",
        "set +e; probe & wait; echo after",
    ] {
        let result = BashKitBackend
            .run(
                script,
                ScriptLimits::default(),
                vec![ProbeCommand::new(Behavior::Fatal)],
                vec![],
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.status, ScriptStatus::Error, "{result:?}");
        assert!(result.stderr.contains("fatal inner call"), "{result:?}");
        assert!(!result.stdout.contains("after"), "{result:?}");
    }
}

struct InspectCommand;

#[async_trait]
impl ScriptCommand for InspectCommand {
    fn name(&self) -> &str {
        "inspect"
    }

    async fn run(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        if args == ["--fail"] {
            return Ok(ScriptCommandOutput {
                stderr: "ordinary failure\n".into(),
                exit_code: 7,
                ..Default::default()
            });
        }
        assert_eq!(args, ["two words"]);
        assert_eq!(stdin, Some(b"one\ntwo\n".as_slice()));
        assert_eq!(ctx.env.get("MODE").map(String::as_str), Some("inspect"));
        assert_eq!(ctx.cwd, Path::new("/tmp"));
        assert_eq!(
            ctx.fs.read_file(Path::new("/tmp/input.txt")).await.unwrap(),
            b"fixture\n"
        );
        assert!(!ctx.cancel.is_cancelled());
        Ok(ScriptCommandOutput {
            stdout: "{\"value\":\"two words\"}\n".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn registered_command_receives_args_stdin_env_and_vfs_in_a_pipeline() {
    // Exercise the object-safe backend interface used by runtime callers too.
    let backend: &dyn ScriptBackend = &BashKitBackend;
    let result = backend.run(
        "export MODE=inspect\necho fixture > input.txt\nprintf 'one\\ntwo\\n' | inspect 'two words' | jq -r '.value'",
        ScriptLimits::default(), vec![Arc::new(InspectCommand)], vec![], CancellationToken::new(),
    ).await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "two words\n");
    assert!(result.stderr.is_empty());
}

#[tokio::test]
async fn registered_command_nonzero_exit_is_recoverable_with_set_plus_e() {
    let result = BashKitBackend
        .run(
            "set +e; inspect --fail; echo $?",
            ScriptLimits::default(),
            vec![Arc::new(InspectCommand)],
            vec![],
            CancellationToken::new(),
        )
        .await;
    assert_eq!(result.status, ScriptStatus::Ok, "{result:?}");
    assert_eq!(result.stdout, "7\n");
    assert_eq!(result.stderr, "ordinary failure\n");
}
