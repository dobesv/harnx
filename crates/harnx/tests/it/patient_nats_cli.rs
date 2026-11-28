//! Frontend coverage for the patient managed-local broker (plan #2922, T2):
//! every CLI path runs inside the `run_main` broker scope, so a slow local
//! NATS must show stderr wait notices (including with `--final-only`),
//! stay cancellable through Ctrl-C, and clean up its child so the next
//! process can take over the lock.
//!
//! Slowness comes from a `NATS_SERVER_BIN` wrapper that delays the port
//! report until a gate file appears; `HARNX_TEST_BROKER_NOTICE_MS` shrinks
//! the notice cadence so these tests need no multi-second sleeps.

use anyhow::{bail, Context, Result};
use harnx::test_utils::interrupt::{
    assert_process_reaped, spawn_harnx_subcommand_with_envs, spawn_oneshot_with_envs,
    wait_for_exit, write_minimal_config, ConfigPaths, GatedNats,
};
use harnx::test_utils::mock_openai_server::{MockOpenAiScript, MockOpenAiServer, MockOpenAiTurn};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

#[cfg(test)]
mod subcommand_admission;

const NOTICE_SEAM_MS: &str = "200";
const EXIT_BUDGET: Duration = Duration::from_secs(10);
const TURN_BUDGET: Duration = Duration::from_secs(60);

/// Streams a child pipe line-by-line on a reader thread so the test can
/// assert on output while the child is still running.
struct OutputPump {
    receiver: Receiver<String>,
    handle: std::thread::JoinHandle<()>,
}

impl OutputPump {
    fn start<R: Read + Send + 'static>(pipe: R) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        let handle = std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { receiver, handle }
    }

    fn wait_for_line(&self, needle: &str, budget: Duration) -> Result<String> {
        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.receiver.recv_timeout(remaining) {
                Ok(line) if line.contains(needle) => return Ok(line),
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    bail!("no output line matching {needle:?} within {budget:?}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("output closed before a line matching {needle:?} appeared")
                }
            }
        }
    }

    fn drain(&self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(line) = self.receiver.try_recv() {
            lines.push(line);
        }
        lines
    }

    fn finish(self) {
        let _ = self.handle.join();
    }
}

struct ChildPipes {
    child: Child,
    stdout: OutputPump,
    stderr: OutputPump,
}

fn spawn_one_shot_piped(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    prompt: &str,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<ChildPipes> {
    let env_refs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let child = spawn_oneshot_with_envs(paths, harnx_bin, prompt, args, &env_refs)?;
    pipes_for(child)
}

fn spawn_subcommand_piped(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<ChildPipes> {
    let env_refs: Vec<(&str, &str)> = envs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let child = spawn_harnx_subcommand_with_envs(paths, harnx_bin, args, &env_refs)?;
    pipes_for(child)
}

fn pipes_for(mut child: Child) -> Result<ChildPipes> {
    let stdout = OutputPump::start(child.stdout.take().context("stdout piped")?);
    let stderr = OutputPump::start(child.stderr.take().context("stderr piped")?);
    Ok(ChildPipes {
        child,
        stdout,
        stderr,
    })
}

fn sigint(child: &Child) {
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(
        result,
        0,
        "SIGINT delivery failed: {}",
        std::io::Error::last_os_error()
    );
}

fn mock_single_turn(text: &str) -> MockOpenAiServer {
    MockOpenAiServer::start(MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec![text.to_string()],
            ..Default::default()
        }],
        ..Default::default()
    })
    .expect("start mock OpenAI server")
}

/// Ordinary Cmd new-session creation through a slow managed-local startup:
/// `--final-only` keeps stdout machine-readable while broker wait notices
/// go to stderr, Ctrl-C cancels the reservation wait, the owned child is
/// reaped before the lock is released, and a second process acquires
/// ownership and completes the turn once the gate opens.
#[test]
fn nats_slow_final_only_cmd_reports_on_stderr_cancels_and_hands_off() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    let mock = mock_single_turn("NATS final smoke");
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let mut first = spawn_one_shot_piped(
        &paths,
        &harnx_bin,
        "hello slow broker",
        &["--final-only"],
        &gate.child_env(NOTICE_SEAM_MS),
    )?;
    let first_notice = first
        .stderr
        .wait_for_line("Waiting for nats-server", Duration::from_secs(20))?;
    assert!(
        first_notice.contains("report its port"),
        "notice must name the observed phase: {first_notice}"
    );
    assert!(
        first_notice.contains("Ctrl-C"),
        "notice must explain how to stop waiting: {first_notice}"
    );
    // The repeat notice proves the cadence keeps reporting while the wait is
    // ongoing. Its rendered text may equal the first notice: elapsed is
    // printed in whole seconds, and the test seam shrinks the cadence to
    // 200ms, so both notices legitimately read "(0s elapsed)".
    let second_notice = first
        .stderr
        .wait_for_line("Waiting for nats-server", Duration::from_secs(5))?;
    assert!(
        second_notice.contains("Press Ctrl-C"),
        "repeat notice must keep explaining how to stop waiting: {second_notice}"
    );
    assert!(
        first.stdout.drain().is_empty(),
        "final-only stdout must stay clean while waiting"
    );
    let broker_pid = gate.started(0)?;
    assert!(
        first.child.try_wait()?.is_none(),
        "waiting frontend must not exit on its own"
    );

    sigint(&first.child);
    let status = wait_for_exit(&mut first.child, EXIT_BUDGET)?;
    let stderr = first.stderr.drain().join("\n");
    first.stdout.finish();
    first.stderr.finish();
    assert_eq!(
        status.code(),
        Some(1),
        "cancelled one-shot exits 1: {stderr}"
    );
    assert!(
        stderr.contains("cancelled while waiting for NATS"),
        "cancellation must wake the reservation wait: {stderr}"
    );
    // Shutdown was awaited before the process exited: the owned broker child
    // is reaped, so the lock it held is available to the next process.
    assert_process_reaped(broker_pid);

    gate.allow()?;
    let mut second = spawn_one_shot_piped(
        &paths,
        &harnx_bin,
        "retry after slow start",
        &["--final-only"],
        &gate.child_env(NOTICE_SEAM_MS),
    )?;
    let status = wait_for_exit(&mut second.child, TURN_BUDGET)?;
    let stdout = second.stdout.drain().join("\n");
    let stderr = second.stderr.drain().join("\n");
    second.stdout.finish();
    second.stderr.finish();
    assert!(status.success(), "second run failed: {stderr}");
    assert_eq!(
        stdout, "NATS final smoke",
        "final-only stdout must contain exactly the durable response"
    );
    let replacement = gate.started(1)?;
    assert_ne!(
        replacement, broker_pid,
        "second process must own a new child"
    );
    Ok(())
}

/// Bare `--session` in Cmd mode reserves a session id after the agent is
/// active; that preparatory wait is patient and cancellable rather than
/// failing through the reservation's outer budget.
#[test]
fn nats_bare_session_cmd_waits_patiently_and_ctrl_c_cancels() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    let mock = mock_single_turn("bare session smoke");
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let mut first = spawn_one_shot_piped(
        &paths,
        &harnx_bin,
        "bare session slow broker",
        &["--final-only", "--session"],
        &gate.child_env(NOTICE_SEAM_MS),
    )?;
    first
        .stderr
        .wait_for_line("Waiting for nats-server", Duration::from_secs(20))?;
    let broker_pid = gate.started(0)?;
    assert!(
        first.child.try_wait()?.is_none(),
        "bare --session must wait for the broker, not fail from the reservation budget"
    );

    sigint(&first.child);
    let status = wait_for_exit(&mut first.child, EXIT_BUDGET)?;
    let stderr = first.stderr.drain().join("\n");
    first.stdout.finish();
    first.stderr.finish();
    assert_eq!(
        status.code(),
        Some(1),
        "cancelled bare --session exits 1: {stderr}"
    );
    assert!(
        stderr.contains("cancelled while waiting for NATS"),
        "cancellation must wake the bare-session reservation: {stderr}"
    );
    assert_process_reaped(broker_pid);
    Ok(())
}

/// `harnx list sessions` routes through the same frontend scope: the
/// managed-local wait sits outside the 10-second listing budget, Ctrl-C
/// cancels it cleanly, and a rerun succeeds once the broker is up.
#[test]
fn nats_slow_list_sessions_cmd_waits_cancels_then_lists() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let mut first = spawn_subcommand_piped(
        &paths,
        &harnx_bin,
        &["list", "sessions"],
        &gate.child_env(NOTICE_SEAM_MS),
    )?;
    first
        .stderr
        .wait_for_line("Waiting for nats-server", Duration::from_secs(20))?;
    let broker_pid = gate.started(0)?;
    // Still pending well past the former 10s listing budget... proven at the
    // seam cadence by remaining alive; the budget itself was pinned by T1's
    // runtime tests. Here: no failure while the broker is merely slow.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        first.child.try_wait()?.is_none(),
        "listing must wait for the broker, not fail from its operation budget"
    );
    first
        .stderr
        .wait_for_line("Waiting for nats-server", Duration::from_secs(5))?;

    sigint(&first.child);
    let status = wait_for_exit(&mut first.child, EXIT_BUDGET)?;
    let stderr = first.stderr.drain().join("\n");
    first.stdout.finish();
    first.stderr.finish();
    assert_eq!(
        status.code(),
        Some(1),
        "cancelled listing exits 1: {stderr}"
    );
    assert!(
        stderr.contains("cancelled while waiting for NATS"),
        "cancellation must wake the listing wait: {stderr}"
    );
    assert_process_reaped(broker_pid);

    gate.allow()?;
    let mut second = spawn_subcommand_piped(
        &paths,
        &harnx_bin,
        &["list", "sessions"],
        &gate.child_env(NOTICE_SEAM_MS),
    )?;
    let status = wait_for_exit(&mut second.child, TURN_BUDGET)?;
    let stdout = second.stdout.drain().join("\n");
    let stderr = second.stderr.drain().join("\n");
    second.stdout.finish();
    second.stderr.finish();
    assert!(status.success(), "listing rerun failed: {stderr}");
    assert!(
        stdout.trim().is_empty(),
        "no sessions yet; stdout must be the empty list, got: {stdout:?}"
    );
    gate.started(1)?;
    Ok(())
}

/// `harnx list tools` goes through the bounded tool-worker activation: the
/// managed-local acquisition before it is patient, Ctrl-C cancels, and the
/// routed operation completes once the broker is ready.
#[test]
fn nats_tool_routing_waits_patiently_cancels_then_completes() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let mut first = spawn_subcommand_piped(
        &paths,
        &harnx_bin,
        &["list", "tools", "--json"],
        &gate.child_env(NOTICE_SEAM_MS),
    )?;
    first
        .stderr
        .wait_for_line("Waiting for nats-server", Duration::from_secs(20))?;
    let broker_pid = gate.started(0)?;
    assert!(
        first.child.try_wait()?.is_none(),
        "tool routing must wait for the broker, not fail from the worker budget"
    );

    sigint(&first.child);
    let status = wait_for_exit(&mut first.child, EXIT_BUDGET)?;
    let stderr = first.stderr.drain().join("\n");
    first.stdout.finish();
    first.stderr.finish();
    assert_eq!(
        status.code(),
        Some(1),
        "cancelled tool routing exits 1: {stderr}"
    );
    assert!(
        stderr.contains("cancelled while waiting for NATS"),
        "cancellation must wake the tool-routing wait: {stderr}"
    );
    assert_process_reaped(broker_pid);

    gate.allow()?;
    let mut second = spawn_subcommand_piped(
        &paths,
        &harnx_bin,
        &["list", "tools", "--json"],
        &gate.child_env(NOTICE_SEAM_MS),
    )?;
    let status = wait_for_exit(&mut second.child, TURN_BUDGET)?;
    let stdout = second.stdout.drain().join("\n");
    let stderr = second.stderr.drain().join("\n");
    second.stdout.finish();
    second.stderr.finish();
    assert!(status.success(), "tool routing rerun failed: {stderr}");
    // Machine-readable stdout stays parseable on the routed path.
    let _tools: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("tool listing stdout must be JSON, got: {stdout:?}"))?;
    Ok(())
}

/// The tool command's actual operation budget starts at readiness, not at
/// process start: with `--timeout-secs 2`, a broker that reports its port
/// only after ~2.5s must not burn the 2s operation budget, and the fast
/// `list tools` operation afterwards completes. If startup were charged to
/// the operation budget, this run would time out before the gate opens.
#[test]
fn nats_slow_startup_stays_outside_operation_budget() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let mut child = spawn_subcommand_piped(
        &paths,
        &harnx_bin,
        &["--timeout-secs", "2", "list", "tools", "--json"],
        &gate.child_env(NOTICE_SEAM_MS),
    )?;
    child
        .stderr
        .wait_for_line("Waiting for nats-server", Duration::from_secs(20))?;
    gate.started(0)?;

    // Startup must outlast the operation budget without it firing.
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        child.child.try_wait()?.is_none(),
        "startup must not be charged to the 2s operation budget"
    );

    gate.allow()?;
    let status = wait_for_exit(&mut child.child, TURN_BUDGET)?;
    let stdout = child.stdout.drain().join("\n");
    let stderr = child.stderr.drain().join("\n");
    child.stdout.finish();
    child.stderr.finish();
    assert!(
        status.success(),
        "operation after slow startup failed: {stderr}"
    );
    let _tools: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("tool listing stdout must be JSON, got: {stdout:?}"))?;
    Ok(())
}

/// The gated broker fixture's child environment plus the per-run test seams.
fn gate_env_with(gate: &GatedNats, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    gate.child_env(NOTICE_SEAM_MS)
        .into_iter()
        .chain(
            extra
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
        )
        .collect()
}

/// Post-readiness stall seams: each budget must fire on the actual operation,
/// not on startup. The gate is open before the child spawns, so the broker is
/// ready almost immediately; the shrunk cutoff (300ms) plus the stall seam
/// (1500ms) then prove the listing budget enforces after readiness without a
/// multi-second sleep. A control run without the seams completes against the
/// same ready broker, so the failure is the budget, not acquisition.
#[test]
fn nats_list_sessions_post_readiness_budget_fires() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    gate.allow()?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let stall_envs = gate_env_with(
        &gate,
        &[
            ("HARNX_TEST_SESSION_LIST_TIMEOUT_MS", "300"),
            ("HARNX_TEST_SESSION_LIST_STALL_MS", "1500"),
        ],
    );
    let mut child = spawn_subcommand_piped(&paths, &harnx_bin, &["list", "sessions"], &stall_envs)?;
    let status = wait_for_exit(&mut child.child, EXIT_BUDGET)?;
    let stderr = child.stderr.drain().join("\n");
    child.stdout.finish();
    child.stderr.finish();
    assert!(!status.success(), "stalled listing must fail: {stderr}");
    assert!(
        stderr.contains("Timed out listing sessions"),
        "the post-readiness listing budget must fire: {stderr}"
    );
    // Error-path cleanup: run_main awaited the broker scope shutdown, so the
    // ready broker child was reaped before the process exited.
    let broker_pid = gate.started(0)?;
    assert_process_reaped(broker_pid);

    // Control: same ready broker, no seams — the plain listing completes.
    let envs = gate.child_env(NOTICE_SEAM_MS);
    let mut control = spawn_subcommand_piped(&paths, &harnx_bin, &["list", "sessions"], &envs)?;
    let status = wait_for_exit(&mut control.child, EXIT_BUDGET)?;
    let stdout = control.stdout.drain().join("\n");
    let stderr = control.stderr.drain().join("\n");
    control.stdout.finish();
    control.stderr.finish();
    assert!(status.success(), "control listing failed: {stderr}");
    assert!(
        stdout.trim().is_empty(),
        "no sessions yet; control stdout must be the empty list, got: {stdout:?}"
    );
    Ok(())
}

/// The session-id reservation budget fires on the reservation operation after
/// broker readiness: with the gate open from the start and the stall seam
/// inside the reservation future, a one-shot Cmd run must fail through the
/// reservation cutoff, not wait forever and not fail from acquisition.
#[test]
fn nats_reservation_post_readiness_budget_fires() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    gate.allow()?;
    let tmp = tempfile::tempdir()?;
    // No model is reached: the run fails at the reservation before any call.
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let stall_envs = gate_env_with(
        &gate,
        &[
            ("HARNX_TEST_RESERVATION_TIMEOUT_MS", "300"),
            ("HARNX_TEST_RESERVATION_STALL_MS", "1500"),
        ],
    );
    let mut child = spawn_one_shot_piped(
        &paths,
        &harnx_bin,
        "hello slow reservation",
        &["--final-only"],
        &stall_envs,
    )?;
    let status = wait_for_exit(&mut child.child, EXIT_BUDGET)?;
    let stderr = child.stderr.drain().join("\n");
    child.stdout.finish();
    child.stderr.finish();
    assert!(!status.success(), "stalled reservation must fail: {stderr}");
    assert!(
        stderr.contains("Timed out reserving a new NATS session ID"),
        "the post-readiness reservation budget must fire: {stderr}"
    );
    let broker_pid = gate.started(0)?;
    assert_process_reaped(broker_pid);
    Ok(())
}

/// The tool-worker activation budget fires on the worker startup handoff after
/// broker readiness: with the gate open and the stall seam inside the
/// activation, `list tools` must fail through the activation cutoff, and the
/// control run without seams proves the ready broker itself is fine.
#[test]
fn nats_worker_activation_post_readiness_budget_fires() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    gate.allow()?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let stall_envs = gate_env_with(
        &gate,
        &[
            ("HARNX_TEST_WORKER_ACTIVATION_TIMEOUT_MS", "300"),
            ("HARNX_TEST_WORKER_ACTIVATION_STALL_MS", "1500"),
        ],
    );
    let mut child = spawn_subcommand_piped(
        &paths,
        &harnx_bin,
        &["list", "tools", "--json"],
        &stall_envs,
    )?;
    let status = wait_for_exit(&mut child.child, EXIT_BUDGET)?;
    let stderr = child.stderr.drain().join("\n");
    child.stdout.finish();
    child.stderr.finish();
    assert!(!status.success(), "stalled activation must fail: {stderr}");
    assert!(
        stderr.contains("Tool worker startup timed out"),
        "the post-readiness worker activation budget must fire: {stderr}"
    );
    let broker_pid = gate.started(0)?;
    assert_process_reaped(broker_pid);

    // Control: same ready broker, no seams — the routed tool listing completes.
    let envs = gate.child_env(NOTICE_SEAM_MS);
    let mut control =
        spawn_subcommand_piped(&paths, &harnx_bin, &["list", "tools", "--json"], &envs)?;
    let status = wait_for_exit(&mut control.child, TURN_BUDGET)?;
    let stdout = control.stdout.drain().join("\n");
    let stderr = control.stderr.drain().join("\n");
    control.stdout.finish();
    control.stderr.finish();
    assert!(status.success(), "control tool listing failed: {stderr}");
    let _tools: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("control tool listing stdout must be JSON, got: {stdout:?}"))?;
    Ok(())
}

/// Frontend bootstrap-error cleanup: a broker wrapper that dies on every
/// attempt makes the frontend's shared acquisition exhaust its launch
/// attempts; the process exits non-zero through the broker scope shutdown,
/// and every child it started is reaped (no leaked brokers hold the lock).
#[test]
fn nats_bootstrap_broker_failure_exhausts_attempts_and_cleans_up() -> Result<()> {
    // Capability probe, same policy as GatedNats.
    let real = match std::env::var_os("NATS_SERVER_BIN") {
        Some(value) if Path::new(&value).is_file() => PathBuf::from(value),
        _ => match which::which("nats-server") {
            Ok(found) => found,
            Err(_) => {
                eprintln!("skipping: nats-server binary not found");
                return Ok(());
            }
        },
    };
    let fixture = tempfile::tempdir()?;
    let root = fixture.path();
    std::fs::create_dir(root.join("real"))?;
    std::os::unix::fs::symlink(real.canonicalize()?, root.join("real/nats-server"))?;
    // Like GatedNats's wrapper but failing immediately: record the PID, then
    // die before binding a port.
    let wrapper = r#"#!/bin/sh
set -eu
root=${0%/*}
if [ "$1" = --version ]; then exec "$root/real/nats-server" "$@"; fi
printf '%s\n' "$$" >> "$root/starts"
exit 23
"#;
    std::fs::write(root.join("nats-server"), wrapper)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        root.join("nats-server"),
        std::fs::Permissions::from_mode(0o700),
    )?;

    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let envs = vec![
        (
            "NATS_SERVER_BIN".to_string(),
            root.join("nats-server").to_string_lossy().into_owned(),
        ),
        (
            "HARNX_TEST_BROKER_NOTICE_MS".to_string(),
            NOTICE_SEAM_MS.to_string(),
        ),
    ];
    let mut child = spawn_subcommand_piped(&paths, &harnx_bin, &["list", "sessions"], &envs)?;
    let status = wait_for_exit(&mut child.child, Duration::from_secs(30))?;
    let stderr = child.stderr.drain().join("\n");
    child.stdout.finish();
    child.stderr.finish();
    assert!(
        !status.success(),
        "a broker that dies on every attempt must fail the frontend: {stderr}"
    );
    assert!(
        stderr.contains("after 5 attempts"),
        "the frontend must surface the exhausted launch attempts: {stderr}"
    );
    // Cleanup: every broker child the frontend started was reaped before the
    // process exited, so a retry is not blocked by a zombie holding the lock.
    let starts = root.join("starts");
    let pids: Vec<u32> = std::fs::read_to_string(&starts)
        .context("wrapper never started")?
        .lines()
        .map(|pid| pid.parse::<u32>().context("wrapper PID"))
        .collect::<Result<_>>()?;
    assert_eq!(pids.len(), 5, "exactly the manager's 5 launch attempts");
    for pid in &pids {
        assert_process_reaped(*pid);
    }
    Ok(())
}
