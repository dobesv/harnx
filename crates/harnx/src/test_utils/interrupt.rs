//! Helpers for interrupt-handling e2e tests (see `tests/it/interrupt_e2e.rs`).
//!
//! This module is compiled only under `cfg(test)` via `src/test_utils/mod.rs`.
//! It intentionally does not expose anything outside the crate.

use crate::test_utils::mock_openai_server::{MockOpenAiScript, MockOpenAiTurn};
use crate::test_utils::tmux_harness::TmuxHarness;
use anyhow::{Context, Result};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A `NATS_SERVER_BIN` wrapper that delays the managed-local broker's port
/// report until a gate file appears, for patient-startup tests. Same shape
/// as `harnx-runtime`'s patient fixtures: exec preserves the wrapper PID,
/// the real server publishes its own ports file, and the gate delays that
/// publication instead of inventing readiness.
#[cfg(unix)]
pub const GATED_NATS_WRAPPER: &str = r#"#!/bin/sh
set -eu
root=${0%/*}
if [ "$1" = --version ]; then exec "$root/real/nats-server" "$@"; fi
printf '%s\n' "$$" >> "$root/starts"
trap 'exit 0' TERM
while [ ! -f "$root/go" ]; do sleep 0.025; done
exec "$root/real/nats-server" "$@"
"#;

/// Gated managed-local broker fixture. `new()` returns `None` when no real
/// nats-server is available (capability probe, not a timeout skip).
#[cfg(unix)]
pub struct GatedNats {
    fixture: tempfile::TempDir,
}

#[cfg(unix)]
impl GatedNats {
    pub fn new() -> Result<Option<Self>> {
        harnx_core::require_nextest();
        let real = match std::env::var_os("NATS_SERVER_BIN") {
            Some(value) if Path::new(&value).is_file() => PathBuf::from(value),
            _ => match which::which("nats-server") {
                Ok(found) => found,
                Err(_) => {
                    eprintln!("skipping: nats-server binary not found");
                    return Ok(None);
                }
            },
        };
        let fixture = tempfile::tempdir().context("broker fixture dir")?;
        let root = fixture.path();
        std::fs::create_dir(root.join("real"))?;
        std::os::unix::fs::symlink(real.canonicalize()?, root.join("real/nats-server"))?;
        std::fs::write(root.join("nats-server"), GATED_NATS_WRAPPER)?;
        std::fs::set_permissions(
            root.join("nats-server"),
            std::fs::Permissions::from_mode(0o700),
        )?;
        Ok(Some(Self { fixture }))
    }

    /// Environment for the spawned harnx child: the gated wrapper plus the
    /// broker-notice cadence seam so tests need no multi-second sleeps.
    pub fn child_env(&self, notice_seam_ms: &str) -> Vec<(String, String)> {
        vec![
            (
                "NATS_SERVER_BIN".to_string(),
                self.wrapper().to_string_lossy().into_owned(),
            ),
            (
                "HARNX_TEST_BROKER_NOTICE_MS".to_string(),
                notice_seam_ms.to_string(),
            ),
        ]
    }

    /// Environment for a tmux pane export, as `KEY=VALUE` strings ready for
    /// `shell_escape`.
    pub fn pane_env(&self, notice_seam_ms: &str) -> Vec<(String, String)> {
        self.child_env(notice_seam_ms)
    }

    pub fn wrapper(&self) -> PathBuf {
        self.fixture.path().join("nats-server")
    }

    pub fn allow(&self) -> Result<()> {
        std::fs::write(self.fixture.path().join("go"), [])?;
        Ok(())
    }

    pub fn close(&self) -> Result<()> {
        std::fs::remove_file(self.fixture.path().join("go"))?;
        Ok(())
    }

    /// PIDs of the wrapper children harnx started (exec preserves them).
    pub fn attempts(&self) -> Vec<u32> {
        std::fs::read_to_string(self.fixture.path().join("starts"))
            .unwrap_or_default()
            .lines()
            .map(|pid| pid.parse().expect("wrapper PID"))
            .collect()
    }

    /// Wait until the wrapper has started `attempt` children.
    pub fn started(&self, attempt: usize) -> Result<u32> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(pid) = self.attempts().get(attempt) {
                return Ok(*pid);
            }
            if Instant::now() >= deadline {
                anyhow::bail!("wrapper child {attempt} never started");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// The child was reaped: its PID is gone entirely (a zombie would still
/// answer `kill(pid, 0)`).
#[cfg(unix)]
pub fn assert_process_reaped(pid: u32) {
    assert_eq!(
        unsafe { libc::kill(pid as libc::pid_t, 0) },
        -1,
        "PID {pid} is still present"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

/// Spinner frames used by the TUI when an LLM/tool/hook call is in flight.
/// Mirrors `SPINNER_FRAMES` in `src/tui/types.rs`. Used to detect whether
/// the TUI is currently busy.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Locate the `harnx-time-tools` binary given the already-resolved `harnx`
/// binary path.
///
/// The `harnx-time-tools` binary lives in its own crate, so
/// `CARGO_BIN_EXE_harnx-time-tools` isn't visible from `harnx`'s test context.
/// Because Cargo builds every workspace member into the same target dir, the
/// binary sits next to the `harnx` binary. Callers are responsible for
/// ensuring it has been built (e.g. via `cargo build --workspace` or
/// `cargo nextest run --workspace`, which is what CI uses).
pub fn harnx_time_tools_bin(harnx_bin: &Path) -> PathBuf {
    let ext = std::env::consts::EXE_SUFFIX;
    harnx_bin.with_file_name(format!("harnx-time-tools{ext}"))
}

/// Locate the `harnx-mcp-repro249` binary given the already-resolved `harnx`
/// binary path.
///
/// The `harnx-mcp-repro249` binary lives in the `harnx-test-bins` crate, so
/// `CARGO_BIN_EXE_harnx-mcp-repro249` isn't visible from `harnx`'s test
/// context. Because Cargo builds every workspace member into the same target
/// dir, the binary sits next to the `harnx` binary. Callers are responsible
/// for ensuring it has been built (e.g. via `cargo build --workspace` or
/// `cargo nextest run --workspace`, which is what CI uses).
pub fn harnx_mcp_repro249_bin(harnx_bin: &Path) -> PathBuf {
    let ext = std::env::consts::EXE_SUFFIX;
    harnx_bin.with_file_name(format!("harnx-mcp-repro249{ext}"))
}

/// A mock-LLM response that emits one short text chunk and immediately
/// issues a `wait` tool call (chunk_delay_ms is 0 so the tool call fires
/// without delay).
pub fn script_call_wait_tool(seconds: u32) -> MockOpenAiScript {
    use crate::test_utils::mock_openai_server::MockOpenAiToolCall;
    MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["Waiting...".to_string()],
            tool_calls: vec![MockOpenAiToolCall {
                encrypted_content: None,
                name: "time_wait".to_string(),
                arguments: serde_json::json!({ "seconds": seconds }),
                id: None,
            }],
            expect: None,
            error: None,
        }],
        fallback_text: "wait-tool script exhausted".to_string(),
        chunk_delay_ms: 0,
    }
}

/// A mock-LLM response that emits one short chunk then holds the stream
/// open. The per-chunk delay is applied between chunks, so the harness
/// sees the first chunk almost immediately and then stalls.
pub fn script_stall_streaming() -> MockOpenAiScript {
    MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec![
                "Thinking".to_string(),
                ".".to_string(),
                ".".to_string(),
                ".".to_string(),
            ],
            tool_calls: vec![],
            expect: None,
            error: None,
        }],
        fallback_text: "stall script exhausted".to_string(),
        chunk_delay_ms: 30_000,
    }
}

/// A mock-LLM response that emits a quick first chunk, then a `SENTINEL_END`
/// chunk after a delay. Used by sub-agent cancellation tests: if cancel
/// propagation works, the child's mock stream is closed before the sentinel
/// is written and `SENTINEL_END` never reaches the parent transcript. If
/// cancellation does NOT propagate (the bug), the child keeps reading from
/// the mock and `SENTINEL_END` eventually appears.
pub fn script_streaming_with_sentinel() -> MockOpenAiScript {
    MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["tick-first".to_string(), "SENTINEL_END".to_string()],
            tool_calls: vec![],
            expect: None,
            error: None,
        }],
        fallback_text: "sentinel script exhausted".to_string(),
        chunk_delay_ms: 1_500,
    }
}

pub struct ConfigPaths {
    pub dir: PathBuf,
    pub harnx_config_dir: PathBuf,
    pub harnx_data_dir: PathBuf,
    pub harnx_state_dir: PathBuf,
}

/// Writes a minimal HARNX_CONFIG_DIR at `<dir>/harnx-config` targeting
/// the given mock OpenAI base URL (e.g. `http://127.0.0.1:<port>/v1`).
pub fn write_minimal_config(dir: &Path, mock_base_url: &str) -> Result<ConfigPaths> {
    let harnx_config_dir = dir.join("harnx-config");
    let harnx_data_dir = dir.join("harnx-data");
    let harnx_state_dir = dir.join("harnx-state");

    std::fs::create_dir_all(harnx_config_dir.join("clients"))
        .context("failed to create harnx config dir")?;
    std::fs::create_dir_all(&harnx_data_dir).context("failed to create harnx data dir")?;
    std::fs::create_dir_all(&harnx_state_dir).context("failed to create harnx state dir")?;

    std::fs::write(
        harnx_config_dir.join("config.yaml"),
        "save: false\nclient: mock-llm\nmodel: mock-llm:test\ntool_use: true\nuse_tools: '*'\n",
    )
    .context("failed to write config.yaml")?;
    std::fs::write(
        harnx_config_dir.join("clients/mock-llm.yaml"),
        format!(
            "type: openai-compatible\nname: mock-llm\napi_base: {mock_base_url}\napi_key: test-key\nmodels:\n  - name: test\n    max_input_tokens: 32000\n    max_output_tokens: 4096\n    supports_tool_use: true\n"
        ),
    )
    .context("failed to write clients/mock-llm.yaml")?;
    // Write a default agent so TUI tests can launch with --agent default
    // and bypass the agent/session pickers (required since both must be set
    // before chat activity is allowed).
    std::fs::create_dir_all(harnx_config_dir.join("agents"))
        .context("failed to create agents dir")?;
    std::fs::write(
        harnx_config_dir.join("agents/default.md"),
        "---\nclient: mock-llm\nmodel: mock-llm:test\ntool_use: true\nuse_tools: '*'\n---\nDefault test agent.\n",
    )
    .context("failed to write agents/default.md")?;
    Ok(ConfigPaths {
        dir: dir.to_path_buf(),
        harnx_config_dir,
        harnx_data_dir,
        harnx_state_dir,
    })
}

/// A mock-LLM response that emits one short text chunk and one tool call
/// with a 1-second wait. Used to exercise the PreToolUse hook path without
/// risking a 30-second hang if cancellation fails.
pub fn script_call_trivial_tool() -> MockOpenAiScript {
    use crate::test_utils::mock_openai_server::MockOpenAiToolCall;
    MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["Listing...".to_string()],
            tool_calls: vec![MockOpenAiToolCall {
                encrypted_content: None,
                name: "time_wait".to_string(),
                arguments: serde_json::json!({ "seconds": 1 }),
                id: None,
            }],
            expect: None,
            error: None,
        }],
        fallback_text: "trivial-tool script exhausted".to_string(),
        chunk_delay_ms: 0,
    }
}

/// Like `write_minimal_config`, but also registers the workspace-built
/// time server through the NATS MCP bridge so the `wait` tool is available.
///
/// `time_tools_bin` should be the path to the compiled `harnx-time-tools`
/// binary, typically obtained via the `harnx_time_tools_bin(harnx_bin)` helper
/// in the calling test. The helper computes the sibling path from the resolved
/// `harnx` binary because Cargo doesn't expose another package's binary through
/// `CARGO_BIN_EXE_*`.
pub fn write_with_wait_tool(
    dir: &Path,
    mock_base_url: &str,
    time_tools_bin: &Path,
) -> Result<ConfigPaths> {
    let paths = write_minimal_config(dir, mock_base_url)?;
    let tool_servers_dir = paths.harnx_config_dir.join("tool_servers");
    std::fs::create_dir_all(&tool_servers_dir).context("failed to create tool_servers dir")?;
    let bridge_bin =
        time_tools_bin.with_file_name(format!("harnx-mcp-bridge{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(
        tool_servers_dir.join("time.yaml"),
        format!(
            "command: {}\nargs:\n  - --name\n  - time\n  - --\n  - {}\n  - --mcp-stdio\n",
            bridge_bin.display(),
            time_tools_bin.display()
        ),
    )
    .context("failed to write tool_servers/time.yaml")?;
    Ok(paths)
}

/// Add middleware that calls a blocking Claude hook through the compatibility
/// tool server. Its timeout exceeds the Ctrl-C budget so only cancellation can
/// stop the sleeping script.
pub fn write_with_blocking_hook(
    dir: &Path,
    mock_base_url: &str,
    time_tools_bin: &Path,
) -> Result<ConfigPaths> {
    let paths = write_with_wait_tool(dir, mock_base_url, time_tools_bin)?;
    let block_sh = paths.dir.join("block.sh");
    let sentinel = paths.dir.join("hook_fired");
    std::fs::write(
        &block_sh,
        format!("#!/bin/sh\ntouch {}\nsleep 30\n", sentinel.display()),
    )
    .context("failed to write block.sh")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&block_sh)?.permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&block_sh, perm)?;
    }
    let tool_servers_dir = paths.harnx_config_dir.join("tool_servers");
    let bridge_bin =
        time_tools_bin.with_file_name(format!("harnx-mcp-bridge{}", std::env::consts::EXE_SUFFIX));
    let claude_hook_bin = time_tools_bin.with_file_name(format!(
        "harnx-claude-hook-tools{}",
        std::env::consts::EXE_SUFFIX
    ));
    std::fs::write(
        tool_servers_dir.join("claude_hook.yaml"),
        format!(
            "command: {}\nargs:\n  - --name\n  - claude_hook\n  - --\n  - {}\n  - --mcp-stdio\n",
            bridge_bin.display(),
            claude_hook_bin.display()
        ),
    )
    .context("failed to write tool_servers/claude_hook.yaml")?;

    std::fs::write(
        paths.harnx_config_dir.join("config.yaml"),
        format!(
            "save: false\nclient: mock-llm\nmodel: mock-llm:test\ntool_use: true\nuse_tools: '*'\n\
             middleware:\n  - name: block\n    match: '^time_wait$'\n    tools: [claude_hook_run]\n    timeout: 300s\n    bash: |\n      claude_hook_run --json '{{\"command\":[{}],\"event\":\"PreToolUse\",\"payload\":{{\"session_id\":\"test\",\"tool_name\":\"time_wait\"}}}}'\n      next\n",
            serde_json::to_string(&block_sh.display().to_string())?
        ),
    )
    .context("failed to write config.yaml with hook")?;
    Ok(paths)
}

/// Starts tmux + bash, exports `HARNX_CONFIG_DIR`, and launches harnx in
/// TUI mode. Returns the harness once the TUI input area appears.
///
/// `harnx_bin` should be the path to the compiled harnx binary, typically
/// obtained via `PathBuf::from(env!("CARGO_BIN_EXE_harnx"))` in the calling
/// test (the `env!` macro for `CARGO_BIN_EXE_*` is only available in
/// integration-test compilation units, not in library code).
///
/// `repo_root` is used as the working directory for the tmux session; pass
/// `PathBuf::from(env!("CARGO_MANIFEST_DIR"))` from the test.
pub fn spawn_tui(paths: &ConfigPaths, harnx_bin: &Path, repo_root: &Path) -> Result<TmuxHarness> {
    spawn_tui_with_envs(paths, harnx_bin, repo_root, &[])
}

/// Like [`spawn_tui`], but exports extra environment variables (e.g. a gated
/// `NATS_SERVER_BIN` wrapper and the broker-notice test seam) before starting
/// harnx.
pub fn spawn_tui_with_envs(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    repo_root: &Path,
    envs: &[(&str, &str)],
) -> Result<TmuxHarness> {
    let tmux = TmuxHarness::new(repo_root, 120, 35).context("failed to create tmux session")?;
    let mut export_line = format!(
        "export HARNX_CONFIG_DIR={} HARNX_DATA_DIR={} HARNX_STATE_DIR={}\n",
        shell_escape(&paths.harnx_config_dir.to_string_lossy()),
        shell_escape(&paths.harnx_data_dir.to_string_lossy()),
        shell_escape(&paths.harnx_state_dir.to_string_lossy())
    );
    for (key, value) in envs {
        export_line = format!("{export_line}export {key}={}\n", shell_escape(value));
    }
    tmux.send_text(&export_line)?;
    // Pass --agent default --session so the TUI starts with both an agent
    // and a session already active, bypassing the agent/session pickers.
    // The "default" agent is written by write_minimal_config.
    tmux.send_text(&format!(
        "{} --agent default --session || echo HARNX_EXIT:$?\n",
        shell_escape(&harnx_bin.to_string_lossy())
    ))?;
    // Wait for the TUI to paint its input area. When no status info is
    // available the input title reads "Input"; when an agent/session is
    // active the title shows the session status line instead.  Both paths
    // render a "•" (idle spinner) or a spinner frame before the title, so
    // we look for the common idle indicator OR the "Input" fallback string.
    tmux.wait_for(Duration::from_secs(15), |screen| {
        screen.contains("Input")
            || screen.contains("• 🤖")
            || SPINNER_FRAMES.iter().any(|f| screen.contains(*f))
    })
    .context("TUI did not start (no Input header after 15s)")?;
    Ok(tmux)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Starts tmux + bash, exports `HARNX_CONFIG_DIR`, and launches harnx in
/// one-shot (Cmd) mode with `prompt` as the prompt-subcommand input. Returns
/// the harness
/// immediately; the harnx process runs inside the pane and the exit code will
/// appear as `HARNX_EXIT:<code>` when it finishes.
///
/// Use [`wait_for_cmd_exit`] to block until harnx exits and retrieve whether
/// the exit was non-zero.
///
/// `repo_root` is used as the working directory for the tmux session; pass
/// `PathBuf::from(env!("CARGO_MANIFEST_DIR"))` from the test.
pub fn spawn_oneshot_in_tmux(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    prompt: &str,
    repo_root: &Path,
) -> Result<TmuxHarness> {
    let tmux = TmuxHarness::new(repo_root, 120, 35).context("failed to create tmux session")?;
    tmux.send_text(&format!(
        "export HARNX_CONFIG_DIR={} HARNX_DATA_DIR={} HARNX_STATE_DIR={}\n",
        shell_escape(&paths.harnx_config_dir.to_string_lossy()),
        shell_escape(&paths.harnx_data_dir.to_string_lossy()),
        shell_escape(&paths.harnx_state_dir.to_string_lossy())
    ))?;
    // "|| echo HARNX_EXIT:$?" prints a detectable sentinel when harnx exits
    // non-zero (interrupted), making exit detection possible from pane capture.
    // --agent default is required since chat activity needs an active agent.
    tmux.send_text(&format!(
        "{} prompt --agent default {} || echo HARNX_EXIT:$?\n",
        shell_escape(&harnx_bin.to_string_lossy()),
        shell_escape(prompt),
    ))?;
    Ok(tmux)
}

/// Waits until `HARNX_EXIT:` appears in the tmux pane (indicating the harnx
/// one-shot command has finished) and returns whether the exit was non-zero.
/// Returns `Err` if the budget elapses without seeing the sentinel.
pub fn wait_for_cmd_exit(tmux: &TmuxHarness, budget: Duration) -> Result<bool> {
    let deadline = Instant::now() + budget;
    loop {
        let screen = tmux.capture_pane()?;
        if let Some(line) = screen.lines().find(|l| l.contains("HARNX_EXIT:")) {
            // "HARNX_EXIT:1" -> non-zero (interrupted); "HARNX_EXIT:0" -> success
            let nonzero = line
                .split("HARNX_EXIT:")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .map(|code| code != "0")
                .unwrap_or(true);
            return Ok(nonzero);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "HARNX_EXIT not seen in tmux pane after {:?}; last screen:\n{}",
                budget,
                screen
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
/// Polls the pane until no SPINNER_FRAME char is visible in the most
/// recent ~10 lines, indicating the harness is idle and ready for new
/// input. Returns Err if the budget elapses while a spinner is still
/// visible.
pub fn wait_for_prompt_return(tmux: &TmuxHarness, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        let screen = tmux.capture_pane()?;
        let tail: String = screen.lines().rev().take(10).collect::<Vec<_>>().join("\n");
        if !tail.chars().any(|c| SPINNER_FRAMES.contains(&c)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "spinner still visible after {:?}; last screen tail:\n{tail}",
                budget
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Spawns `harnx prompt "<prompt>"` non-interactively. Returns the `Child` —
/// caller is responsible for `wait_for_exit` or kill.
pub fn spawn_oneshot(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    prompt: &str,
) -> Result<std::process::Child> {
    spawn_oneshot_with_args(paths, harnx_bin, prompt, &[])
}

/// Spawns a final-response-only one-shot command.
pub fn spawn_oneshot_final_only(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    prompt: &str,
) -> Result<std::process::Child> {
    spawn_oneshot_with_args(paths, harnx_bin, prompt, &["--final-only"])
}

/// Spawns a one-shot command with extra CLI arguments before the prompt separator.
pub fn spawn_oneshot_with_args(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    prompt: &str,
    extra_args: &[&str],
) -> Result<std::process::Child> {
    spawn_oneshot_with_envs(paths, harnx_bin, prompt, extra_args, &[])
}

/// Spawns a one-shot command with extra CLI arguments and extra environment
/// variables (e.g. a gated `NATS_SERVER_BIN` wrapper or the broker-notice
/// test seam).
pub fn spawn_oneshot_with_envs(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    prompt: &str,
    extra_args: &[&str],
    envs: &[(&str, &str)],
) -> Result<std::process::Child> {
    harnx_core::require_nextest();
    let mut command = one_shot_command(paths, harnx_bin, prompt, extra_args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.spawn().context("failed to spawn harnx one-shot")
}

/// Spawns a harnx subcommand (not the one-shot prompt path) with extra
/// environment variables, with piped output for assertion.
pub fn spawn_harnx_subcommand_with_envs(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<std::process::Child> {
    harnx_core::require_nextest();
    let mut command = Command::new(harnx_bin);
    command
        .args(args)
        .env("HARNX_CONFIG_DIR", &paths.harnx_config_dir)
        .env("HARNX_DATA_DIR", &paths.harnx_data_dir)
        .env("HARNX_STATE_DIR", &paths.harnx_state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    command.spawn().context("failed to spawn harnx subcommand")
}

fn one_shot_command(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    prompt: &str,
    extra_args: &[&str],
) -> Command {
    // --agent default is required since chat activity needs an active agent.
    // The "default" agent is written by write_minimal_config.
    let mut command = Command::new(harnx_bin);
    command
        .args(["prompt", "--agent", "default"])
        .args(extra_args)
        .arg("--")
        .arg(prompt)
        .env("HARNX_CONFIG_DIR", &paths.harnx_config_dir)
        .env("HARNX_DATA_DIR", &paths.harnx_data_dir)
        .env("HARNX_STATE_DIR", &paths.harnx_state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Sends SIGINT to a running child via `libc::kill`. Unix-only.
#[cfg(unix)]
pub fn send_sigint(child: &std::process::Child) -> Result<()> {
    let pid = child.id() as i32;
    let rc = unsafe { libc::kill(pid, libc::SIGINT) };
    if rc != 0 {
        anyhow::bail!(
            "kill({pid}, SIGINT) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}
/// Polls `Child::try_wait` until the child exits or the budget elapses.
pub fn wait_for_exit(
    child: &mut std::process::Child,
    budget: Duration,
) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            anyhow::bail!("child did not exit within {:?}", budget);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::SPINNER_FRAMES;

    #[test]
    fn spinner_frames_match_tui() {
        let expected: Vec<char> = crate::tui::types::SPINNER_FRAMES
            .iter()
            .flat_map(|frame| frame.chars())
            .collect();
        let actual: Vec<char> = SPINNER_FRAMES.to_vec();
        assert_eq!(
            expected, actual,
            "src/test_utils/interrupt.rs SPINNER_FRAMES drifted from src/tui/types.rs"
        );
    }
}
