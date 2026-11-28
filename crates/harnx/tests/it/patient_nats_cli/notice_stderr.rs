//! P1: real stderr pipes must not couple notice output to broker cleanup.

use super::{local_lock, signal_group, ConfigPaths, GatedNats, EXIT_BUDGET, NOTICE_SEAM_MS};
use anyhow::{ensure, Context, Result};
use harnx::test_utils::interrupt::{assert_process_reaped, wait_for_exit, write_minimal_config};
use std::fs::TryLockError;
use std::io::{self, PipeReader, PipeWriter, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// qual:allow(complexity, unsafe) reason: "fcntl is required to fill this OS pipe without changing the blocking mode inherited by the CLI"
fn fill_pipe(writer: &mut PipeWriter) -> io::Result<()> {
    let fd = writer.as_raw_fd();
    // SAFETY: fd is live. Set nonblocking only while filling the pipe in the
    // parent; the CLI inherits a blocking descriptor, as from a normal shell.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        match writer.write(&[b'x'; 512]) {
            Ok(512) => continue,
            Ok(_) => return Err(io::Error::other("unexpected partial pipe write")),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(err),
        }
    }
    // SAFETY: same live fd; restore the blocking mode before spawning.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn spawn_with_stderr(
    gate: &GatedNats,
    paths: &ConfigPaths,
    writer: PipeWriter,
) -> io::Result<std::process::Child> {
    Command::new(env!("CARGO_BIN_EXE_harnx"))
        .args(["info", "session", "default", "test01"])
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
        .stdout(Stdio::null())
        .stderr(Stdio::from(writer))
        .spawn()
}

// qual:allow(complexity, unsafe) reason: "getpgid checks the broker fixture is isolated from the CLI process group before SIGINT"
fn cancel_with_pipe(
    gate: &GatedNats,
    reader: Option<PipeReader>,
    writer: PipeWriter,
) -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let child = spawn_with_stderr(gate, &paths, writer)?;
    let mut child = scopeguard::guard(child, |mut child| {
        let _ = child.kill();
        let _ = child.wait();
        for pid in gate.attempts() {
            let _ = signal_group("KILL", pid);
        }
    });
    let broker_pid = gate.started(0)?;
    let lock = local_lock(&paths)?;
    // Leave stderr full/closed across multiple notice ticks while the gate
    // holds the broker in the port-report phase.
    std::thread::sleep(Duration::from_millis(800));
    assert!(child.try_wait()?.is_none(), "CLI exited before Ctrl-C");
    assert!(matches!(lock.try_lock(), Err(TryLockError::WouldBlock)));
    let cli_pid = child.id();
    assert_ne!(broker_pid, cli_pid);
    // SAFETY: getpgid only observes the PID reported by our broker fixture.
    let broker_group = unsafe { libc::getpgid(broker_pid as libc::pid_t) };
    assert_eq!(broker_group, broker_pid as libc::pid_t);
    signal_group("INT", cli_pid)?;
    let deadline = Instant::now() + EXIT_BUDGET;
    // Keep the pipe undrained until awaited broker cleanup finishes. Closing
    // the reader afterward releases any separate command exit diagnostic.
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                ensure!(
                    Instant::now() < deadline,
                    "Ctrl-C did not release the broker lock while stderr was unavailable"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
    assert_process_reaped(broker_pid);
    lock.unlock()?;
    drop(reader);
    let status = wait_for_exit(
        &mut child,
        deadline.saturating_duration_since(Instant::now()),
    )
    .context("CLI did not exit after broker cleanup")?;
    assert_eq!(
        status.code(),
        Some(1),
        "expected normal cancellation: {status}"
    );
    // `try_wait` cached the status; disarm the failure guard without
    // signalling any PID that the OS could have reused.
    scopeguard::ScopeGuard::into_inner(child).wait()?;
    Ok(())
}

#[test]
fn nats_full_notice_stderr_does_not_delay_broker_cleanup() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    let (reader, mut writer) = io::pipe()?;
    fill_pipe(&mut writer)?;
    cancel_with_pipe(&gate, Some(reader), writer)
}

#[test]
fn nats_broken_notice_stderr_does_not_panic_broker_producer() -> Result<()> {
    let Some(gate) = GatedNats::new()? else {
        return Ok(());
    };
    let (reader, writer) = io::pipe()?;
    drop(reader);
    cancel_with_pipe(&gate, None, writer)
}
