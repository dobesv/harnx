//! Review B4: SIGINT while real non-turn dispatch is waiting for local NATS.

use super::{pipes_for, ChildPipes, EXIT_BUDGET, NOTICE_SEAM_MS, TURN_BUDGET};
use anyhow::{Context, Result};
use harnx::test_utils::interrupt::{
    assert_process_reaped, wait_for_exit, write_minimal_config, ConfigPaths, GatedNats,
};
use harnx_core::config_paths::NATS_RUNTIME_DIR;
use std::fs::{File, TryLockError};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

#[path = "notice_stderr.rs"]
mod notice_stderr;

fn signal_group(signal: &str, pid: u32) -> Result<()> {
    anyhow::ensure!(pid > 1, "invalid process group ID {pid}");
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            "kill -s \"$1\" -- \"-$2\"",
            "kill-group",
            signal,
            &pid.to_string(),
        ])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "signal {signal} to group {pid}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Test failure must not strand the CLI or the separately grouped wrapper on
/// macOS, where the kernel does not signal the child when the parent dies.
struct Invocation<'a> {
    pipes: ChildPipes,
    gate: &'a GatedNats,
    attempt: usize,
    reaped: bool,
}

impl<'a> Invocation<'a> {
    fn spawn(gate: &'a GatedNats, paths: &ConfigPaths, args: &[&str]) -> Result<Self> {
        let attempt = gate.attempts().len();
        let child = Command::new(env!("CARGO_BIN_EXE_harnx"))
            .args(args)
            .envs(gate.child_env(NOTICE_SEAM_MS))
            .env("HARNX_CONFIG_DIR", &paths.harnx_config_dir)
            .env("HARNX_DATA_DIR", &paths.harnx_data_dir)
            .env("HARNX_STATE_DIR", &paths.harnx_state_dir)
            .env_remove("HARNX_NATS_SERVER")
            .env_remove("HARNX_NATS_URL")
            .env_remove("HARNX_NATS_TOKEN")
            .current_dir(&paths.dir)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn harnx {args:?}"))?;
        Ok(Self {
            pipes: pipes_for(child)?,
            gate,
            attempt,
            reaped: false,
        })
    }

    fn interrupt(&self) -> Result<()> {
        // process_group(0) makes the CLI its own group leader.
        signal_group("INT", self.pipes.child.id()).context("send SIGINT to CLI process group")
    }

    fn assert_reaped(&mut self) -> Result<u32> {
        let pid = self.gate.started(self.attempt)?;
        assert_process_reaped(pid);
        self.reaped = true;
        Ok(pid)
    }
}

impl Drop for Invocation<'_> {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.pipes.child.kill();
            let _ = self.pipes.child.wait();
            for &pid in &self.gate.attempts()[self.attempt..] {
                // Stop any sleep launched by a stranded broker wrapper.
                let _ = signal_group("KILL", pid);
            }
        }
    }
}

fn local_lock(paths: &ConfigPaths) -> Result<File> {
    Ok(File::open(
        paths
            .harnx_data_dir
            .join(NATS_RUNTIME_DIR)
            .join("nats.lock"),
    )?)
}

fn cancel_and_take_over(gate: &GatedNats, args: &[&str]) -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let mut first = Invocation::spawn(gate, &paths, args)?;
    let notice = first
        .pipes
        .stderr
        .wait_for_line("report its port", Duration::from_secs(25))
        .with_context(|| format!("CLI stdout: {:?}", first.pipes.stdout.drain()))?;
    assert!(notice.contains("Ctrl-C"), "missing cancel hint: {notice}");
    let broker_pid = gate.started(first.attempt)?;
    assert!(
        first.pipes.child.try_wait()?.is_none(),
        "{args:?} did not wait"
    );
    let lock = local_lock(&paths)?;
    assert!(matches!(lock.try_lock(), Err(TryLockError::WouldBlock)));

    // Model terminal Ctrl-C with SIGINT to the CLI process group, not to the
    // broker. Neither a terminal signal nor macOS parent death reaps it.
    let cli_pid = first.pipes.child.id() as libc::pid_t;
    // SAFETY: getpgid reads the process group of the recorded broker PID.
    assert_eq!(
        unsafe { libc::getpgid(broker_pid as libc::pid_t) },
        broker_pid as libc::pid_t
    );
    assert_ne!(cli_pid, broker_pid as libc::pid_t);
    first.interrupt()?;
    let status = wait_for_exit(&mut first.pipes.child, EXIT_BUDGET)?;
    assert_eq!(
        status.code(),
        Some(1),
        "{args:?} must exit normally through cancellation, not die by signal: {status}"
    );
    first.assert_reaped()?;
    lock.try_lock()
        .context("reacquire broker owner lock immediately after cancellation")?;
    lock.unlock()?;

    gate.allow()?;
    let mut successor = Invocation::spawn(gate, &paths, &["list", "sessions"])?;
    let status = wait_for_exit(&mut successor.pipes.child, TURN_BUDGET)?;
    assert!(
        status.success(),
        "broker takeover failed after {args:?}: {:?}",
        successor.pipes.stderr.drain()
    );
    assert_ne!(successor.assert_reaped()?, broker_pid);
    gate.close()?;
    Ok(())
}

fn exercise(commands: &[&[&str]]) -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    for args in commands {
        cancel_and_take_over(&gate, args).with_context(|| format!("harnx {args:?}"))?;
    }
    Ok(())
}

#[test]
fn nats_session_inspection_and_follow_admission_cancel() -> Result<()> {
    exercise(&[
        &["info", "session", "default", "test01"],
        &["info", "session", "default", "test01", "--format", "json"],
        &["dump", "session", "default", "test01", "--format", "json"],
        &["dump", "session", "default", "test01", "--follow"],
        &[
            "dump",
            "session",
            "default",
            "test01",
            "--check-loop-detection",
        ],
        &["compact", "session", "default", "test01", "--timeout", "0"],
    ])
}

#[test]
fn nats_attachment_admission_cancels() -> Result<()> {
    let session = harnx_core::cid_url::SessionRef::new(None, "test01".into())?;
    let cid = harnx_blob_store::media_cid_url(
        &session,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .to_string();
    exercise(&[&["dump", "attachment", &cid], &["open", "attachment", &cid]])
}

#[test]
fn nats_session_admin_admission_cancels() -> Result<()> {
    exercise(&[
        &[
            "delete",
            "session",
            "test01",
            "--agent",
            "default",
            "--cluster",
            "__local__",
        ],
        &["list", "orphan-streams", "--cluster", "__local__"],
        &["credentials", "list"],
        &[
            "delete",
            "orphan-streams",
            "--cluster",
            "__local__",
            "--grace-hours",
            "1",
        ],
    ])
}

#[test]
fn nats_session_interact_and_surface_admission_cancel() -> Result<()> {
    exercise(&[
        &["interact", "list", "default", "test01"],
        &[
            "interact", "answer", "default", "test01", "--id", "test", "--action", "approve",
        ],
        &["surface", "list", "default", "test01"],
        &[
            "surface", "act", "default", "test01", "--id", "test", "--action", "confirm",
        ],
    ])
}

/// Once attached, Ctrl-C still exits dump's follow loop successfully and
/// leaves a submitted compaction pending with exit 130. The compaction CLI
/// joins the dump CLI's broker and must not take that broker down on exit.
#[test]
fn nats_ready_followers_preserve_their_ctrl_c_exit_status() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    gate.allow()?;
    let mock = super::mock_single_turn("follow-test response");
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let mut seed = Invocation::spawn(
        &gate,
        &paths,
        &[
            "--agent",
            "default",
            "--session",
            "test01",
            "--final-only",
            "prompt",
            "follow-test input",
        ],
    )?;
    let status = wait_for_exit(&mut seed.pipes.child, TURN_BUDGET)?;
    assert!(
        status.success(),
        "failed to seed session: {:?}",
        seed.pipes.stderr.drain()
    );
    seed.assert_reaped()?;

    let mut dump = Invocation::spawn(
        &gate,
        &paths,
        &[
            "dump", "session", "default", "test01", "--follow", "--format", "json",
        ],
    )?;
    dump.pipes
        .stdout
        .wait_for_line("follow-test response", TURN_BUDGET)?;
    let pid = gate.started(dump.attempt)?;
    let mut compact = Invocation::spawn(
        &gate,
        &paths,
        &["compact", "session", "default", "test01", "--timeout", "0"],
    )?;
    // There is no worker now: the seed command reaped its worker, and dump
    // starts only the broker. The durable request remains pending.
    let submitted = compact
        .pipes
        .stdout
        .wait_for_line("Compaction request submitted (id: ", TURN_BUDGET)?;
    let (_, request_id) = submitted
        .trim_end_matches(')')
        .rsplit_once("(id: ")
        .context("compaction request id")?;
    compact.interrupt()?;
    let status = wait_for_exit(&mut compact.pipes.child, EXIT_BUDGET)?;
    assert_eq!(status.code(), Some(130));
    compact.pipes.stdout.wait_for_line(
        &format!("Interrupted. Compaction still pending (request id: {request_id})."),
        EXIT_BUDGET,
    )?;
    assert_eq!(
        gate.attempts().len(),
        compact.attempt,
        "compact must only join"
    );
    assert!(dump.pipes.child.try_wait()?.is_none());
    // SAFETY: signal 0 tests existence without delivering a signal.
    assert_eq!(
        unsafe { libc::kill(pid as libc::pid_t, 0) },
        0,
        "joiner killed owner broker"
    );

    dump.interrupt()?;
    let status = wait_for_exit(&mut dump.pipes.child, EXIT_BUDGET)?;
    assert!(
        status.success(),
        "attached dump follow must exit 0 on Ctrl-C"
    );
    dump.assert_reaped()?;
    let lock = local_lock(&paths)?;
    lock.try_lock()
        .context("reacquire broker lock after dump follow exits")?;
    Ok(())
}
