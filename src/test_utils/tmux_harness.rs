//! Minimal tmux-backed harness for local-only end-to-end TUI tests.
//!
//! This helper intentionally uses the tmux CLI via `std::process` so tests can
//! drive the real terminal UI without introducing additional heavy dependencies.
//! Tests should gate themselves with [`TmuxHarness::is_available`] and skip when
//! tmux is not installed in the local environment.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Small wrapper around an isolated tmux session used for local-only UI tests.
pub struct TmuxHarness {
    session_name: String,
    socket_path: std::path::PathBuf,
    pane_target: String,
}

impl TmuxHarness {
    /// Returns true when tmux appears to be installed and runnable.
    pub fn is_available() -> bool {
        Command::new("tmux")
            .arg("-V")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    /// Creates a new detached tmux session with an isolated socket.
    ///
    /// The shell itself is started in a deterministic test mode: bash skips user
    /// startup files, prompt hooks are cleared, history is disabled, and the prompt
    /// is forced to a stable value. Tests should still apply any custom environment
    /// variables needed by their test subject inside the pane (e.g., via `send_text`
    /// with `env VAR=val ...`).
    pub fn new(cwd: impl AsRef<Path>, cols: u16, rows: u16) -> Result<Self> {
        let session_name = unique_name("harnx-test");
        let socket_path = std::env::temp_dir().join(format!("{}.sock", unique_name("tmux")));

        let mut command = Command::new("tmux");
        command
            .arg("-S")
            .arg(&socket_path)
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&session_name)
            .arg("-x")
            .arg(cols.to_string())
            .arg("-y")
            .arg(rows.to_string())
            .arg("env")
            .arg("-u")
            .arg("BASH_ENV")
            .arg("-u")
            .arg("ENV")
            .arg("PS1=[harnx-test] ")
            .arg("PROMPT_COMMAND=")
            .arg("TERM=dumb")
            .arg("HISTFILE=/dev/null")
            // Bash stays alive in detached mode and accepts `send-keys` input.
            .arg("bash")
            .arg("--noprofile")
            .arg("--norc")
            .arg("-i")
            .current_dir(cwd.as_ref());

        run_command(command).context("failed to create tmux session")?;

        Ok(Self {
            pane_target: format!("{}:0.0", session_name),
            session_name,
            socket_path,
        })
    }

    /// Sends literal text to the tmux pane.
    pub fn send_text(&self, text: &str) -> Result<()> {
        self.tmux(["send-keys", "-t", &self.pane_target, "-l", text])
            .context("failed to send tmux text")?;
        Ok(())
    }

    /// Sends one or more tmux key names, such as `Enter` or `C-l`.
    pub fn send_keys(&self, keys: &[&str]) -> Result<()> {
        let mut args = vec!["send-keys", "-t", &self.pane_target];
        args.extend(keys.iter().copied());
        self.tmux(args).context("failed to send tmux keys")?;
        Ok(())
    }

    /// Captures the pane contents, including scrollback.
    pub fn capture_pane(&self) -> Result<String> {
        let output = self
            .tmux_output([
                "capture-pane",
                "-p",
                "-J",
                "-S",
                "-",
                "-E",
                "-",
                "-t",
                &self.pane_target,
            ])
            .context("failed to capture tmux pane")?;
        String::from_utf8(output.stdout).context("tmux output was not valid UTF-8")
    }

    /// Polls until the pane contains `expected` or times out.
    pub fn wait_for_contains(&self, expected: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;

        loop {
            let last_capture = self.capture_pane()?;
            if last_capture.contains(expected) {
                return Ok(last_capture);
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for tmux pane to contain {:?}. Last capture:\n{}",
                    expected,
                    last_capture
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Polls until the provided predicate matches the captured pane.
    pub fn wait_for<F>(&self, timeout: Duration, predicate: F) -> Result<String>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;

        loop {
            let last_capture = self.capture_pane()?;
            if predicate(&last_capture) {
                return Ok(last_capture);
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for tmux predicate. Last capture:\n{}",
                    last_capture
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn tmux<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_command(self.base_command(args))?;
        Ok(())
    }

    fn tmux_output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self
            .base_command(args)
            .output()
            .context("failed to run tmux")?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(anyhow!(format_command_error("tmux", &output)))
        }
    }

    fn base_command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("tmux");
        command.arg("-S").arg(&self.socket_path);
        for arg in args {
            command.arg(arg.as_ref());
        }
        command
    }
}

impl Drop for TmuxHarness {
    fn drop(&mut self) {
        let _ = self.tmux(["kill-session", "-t", &self.session_name]);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn run_command(mut command: Command) -> Result<Output> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(anyhow!(format_command_error(&program, &output)))
    }
}

fn format_command_error(program: &str, output: &Output) -> String {
    format!(
        "{program} exited with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn unique_name(prefix: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}-{now}")
}
