//! Helpers for interrupt-handling e2e tests (see `tests/interrupt_e2e.rs`).
//!
//! This module is compiled only under `cfg(test)` via `src/test_utils/mod.rs`.
//! It intentionally does not expose anything outside the crate.

use crate::acp::AcpServerConfig;
use crate::test_utils::mock_openai_server::{MockOpenAiScript, MockOpenAiTurn};
use crate::test_utils::tmux_harness::TmuxHarness;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Spinner frames used by the TUI when an LLM/tool/hook call is in flight.
/// Mirrors `SPINNER_FRAMES` in `src/tui/types.rs`. Used to detect whether
/// the TUI is currently busy.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A mock-LLM response that emits one short text chunk and immediately
/// issues a `wait` tool call (chunk_delay_ms is 0 so the tool call fires
/// without delay).
pub fn script_call_wait_tool(seconds: u32) -> MockOpenAiScript {
    use crate::test_utils::mock_openai_server::MockOpenAiToolCall;
    MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["Waiting...".to_string()],
            tool_calls: vec![MockOpenAiToolCall {
                name: "time_wait".to_string(),
                arguments: serde_json::json!({ "seconds": seconds }),
                id: None,
            }],
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
            error: None,
        }],
        fallback_text: "stall script exhausted".to_string(),
        chunk_delay_ms: 30_000,
    }
}

pub struct ConfigPaths {
    pub dir: PathBuf,
    pub harnx_config_dir: PathBuf,
}

/// Writes a minimal HARNX_CONFIG_DIR at `<dir>/harnx-config` targeting
/// the given mock OpenAI base URL (e.g. `http://127.0.0.1:<port>/v1`).
pub fn write_minimal_config(dir: &Path, mock_base_url: &str) -> Result<ConfigPaths> {
    let harnx_config_dir = dir.join("harnx-config");
    std::fs::create_dir_all(harnx_config_dir.join("clients"))
        .context("failed to create harnx config dir")?;
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
    Ok(ConfigPaths {
        dir: dir.to_path_buf(),
        harnx_config_dir,
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
                name: "time_wait".to_string(),
                arguments: serde_json::json!({ "seconds": 1 }),
                id: None,
            }],
            error: None,
        }],
        fallback_text: "trivial-tool script exhausted".to_string(),
        chunk_delay_ms: 0,
    }
}

/// Like `write_minimal_config`, but also registers the workspace-built
/// `harnx-mcp-time` binary as an MCP server so the `wait` tool is available.
///
/// `mcp_time_bin` should be the path to the compiled `harnx-mcp-time` binary,
/// typically obtained via `PathBuf::from(env!("CARGO_BIN_EXE_harnx-mcp-time"))`
/// in the calling test (the `env!` macro for `CARGO_BIN_EXE_*` is only
/// available in integration-test compilation units, not in library code).
pub fn write_with_wait_tool(
    dir: &Path,
    mock_base_url: &str,
    mcp_time_bin: &Path,
) -> Result<ConfigPaths> {
    let paths = write_minimal_config(dir, mock_base_url)?;
    let mcp_servers_dir = paths.harnx_config_dir.join("mcp_servers");
    std::fs::create_dir_all(&mcp_servers_dir).context("failed to create mcp_servers dir")?;
    std::fs::write(
        mcp_servers_dir.join("time.yaml"),
        format!("command: {}\n", mcp_time_bin.display()),
    )
    .context("failed to write mcp_servers/time.yaml")?;
    Ok(paths)
}

/// Like `write_with_wait_tool`, but also overwrites `config.yaml` with a
/// `hooks:` block that registers a PreToolUse hook pointing at a
/// `block.sh` script (runs `sleep 30`) in the same temp dir. The hook's
/// timeout is set to 300s so the harness's per-hook timeout does not kill
/// the hook before our Ctrl-C budget expires.
pub fn write_with_blocking_hook(
    dir: &Path,
    mock_base_url: &str,
    mcp_time_bin: &Path,
) -> Result<ConfigPaths> {
    let paths = write_with_wait_tool(dir, mock_base_url, mcp_time_bin)?;
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
    std::fs::write(
        paths.harnx_config_dir.join("config.yaml"),
        format!(
            "save: false\nclient: mock-llm\nmodel: mock-llm:test\ntool_use: true\nuse_tools: '*'\n\
             hooks:\n  entries:\n    - event: PreToolUse\n      type: claude-command\n      command: {}\n      timeout: 300\n",
            block_sh.display()
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
    let tmux = TmuxHarness::new(repo_root, 120, 35).context("failed to create tmux session")?;
    tmux.send_text(&format!(
        "export HARNX_CONFIG_DIR={}\n",
        shell_escape(&paths.harnx_config_dir.to_string_lossy())
    ))?;
    tmux.send_text(&format!(
        "{} || echo HARNX_EXIT:$?\n",
        shell_escape(&harnx_bin.to_string_lossy())
    ))?;
    // Wait for the TUI to paint its input area. The "• Input" header (or
    // the spinner-frame variant) appears as soon as the TUI starts.
    tmux.wait_for(Duration::from_secs(15), |screen| screen.contains("Input"))
        .context("TUI did not start (no Input header after 15s)")?;
    Ok(tmux)
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A mock-LLM response that emits one short text chunk and immediately
/// issues a `<agent_name>_session_prompt` tool call to delegate to a
/// sub-agent via ACP.
pub fn script_call_sub_agent(agent_name: &str) -> MockOpenAiScript {
    use crate::test_utils::mock_openai_server::MockOpenAiToolCall;
    MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["Delegating...".to_string()],
            tool_calls: vec![MockOpenAiToolCall {
                name: format!("{agent_name}_session_prompt"),
                arguments: serde_json::json!({ "message": "do the thing" }),
                id: None,
            }],
            error: None,
        }],
        fallback_text: "sub-agent script exhausted".to_string(),
        chunk_delay_ms: 0,
    }
}

/// Sets up two config dirs (child and parent) for a sub-agent delegation test.
///
/// - Child dir: `dir/child` — minimal config pointing at `child_mock_url`,
///   plus an agent file at `agents/child.md` (required by `harnx --acp child`).
/// - Parent dir: `dir/parent` — minimal config pointing at `parent_mock_url`,
///   plus an `acp_servers/child.yaml` that spawns another harnx with
///   `--acp child` and `HARNX_CONFIG_DIR` pointing at the child config dir.
///
/// Returns the PARENT's `ConfigPaths` so `spawn_tui` launches the parent.
pub fn write_with_sub_agent(
    dir: &Path,
    parent_mock_url: &str,
    child_mock_url: &str,
    harnx_bin: &Path,
) -> Result<ConfigPaths> {
    // Child config (lives in <dir>/child/harnx-config).
    let child_paths = write_minimal_config(&dir.join("child"), child_mock_url)?;
    // The child needs an agent file matching the agent name passed to --acp.
    std::fs::create_dir_all(child_paths.harnx_config_dir.join("agents"))?;
    std::fs::write(
        child_paths.harnx_config_dir.join("agents/child.md"),
        "---\nname: child\nmodel: mock-llm:test\nuse_tools: '*'\n---\nYou are the child.\n",
    )?;

    // Parent config (lives in <dir>/parent/harnx-config).
    let parent_paths = write_minimal_config(&dir.join("parent"), parent_mock_url)?;

    // ACP server entry on the parent — points at another harnx --acp child
    // with the child's HARNX_CONFIG_DIR.
    let acp_servers_dir = parent_paths.harnx_config_dir.join("acp_servers");
    std::fs::create_dir_all(&acp_servers_dir)?;
    let mut env = HashMap::new();
    env.insert(
        "HARNX_CONFIG_DIR".to_string(),
        child_paths.harnx_config_dir.to_string_lossy().into_owned(),
    );
    let acp_server = AcpServerConfig {
        name: "child".to_string(),
        command: harnx_bin.to_string_lossy().into_owned(),
        args: vec!["--acp".to_string(), "child".to_string()],
        env,
        enabled: true,
        description: None,
        idle_timeout_secs: 60,
        operation_timeout_secs: 60,
    };
    std::fs::write(
        acp_servers_dir.join("child.yaml"),
        serde_yaml::to_string(&acp_server)
            .context("failed to serialize child ACP server config")?,
    )?;

    Ok(parent_paths)
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

/// Spawns `harnx "<prompt>"` non-interactively. Returns the `Child` —
/// caller is responsible for `wait_for_exit` or kill.
pub fn spawn_oneshot(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    prompt: &str,
) -> Result<std::process::Child> {
    use std::process::{Command, Stdio};
    Command::new(harnx_bin)
        .arg(prompt)
        .env("HARNX_CONFIG_DIR", &paths.harnx_config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn harnx one-shot")
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

/// Writes an agent markdown file under `<harnx_config_dir>/agents/<name>.md`
/// pointing at the `mock-llm:test` model with `use_tools: '*'`. Required
/// before launching `harnx --acp <name>`.
pub fn write_acp_agent(paths: &ConfigPaths, name: &str) -> Result<()> {
    let agents = paths.harnx_config_dir.join("agents");
    std::fs::create_dir_all(&agents).context("failed to create agents dir")?;
    std::fs::write(
        agents.join(format!("{name}.md")),
        format!("---\nname: {name}\nmodel: mock-llm:test\nuse_tools: '*'\n---\nYou are {name}.\n"),
    )
    .context("failed to write agent file")?;
    Ok(())
}

/// Builds a connected `AcpClient` against a `harnx --acp <agent>` child
/// using the given config dir.
pub async fn spawn_acp_client(
    paths: &ConfigPaths,
    harnx_bin: &Path,
    agent: &str,
) -> Result<crate::acp::AcpClient> {
    let mut env = HashMap::new();
    env.insert(
        "HARNX_CONFIG_DIR".to_string(),
        paths.harnx_config_dir.to_string_lossy().into_owned(),
    );
    let config = AcpServerConfig {
        name: format!("test-{agent}"),
        command: harnx_bin.to_string_lossy().into_owned(),
        args: vec!["--acp".to_string(), agent.to_string()],
        env,
        enabled: true,
        description: None,
        idle_timeout_secs: 300,
        operation_timeout_secs: 60,
    };
    let client = crate::acp::AcpClient::new(config);
    client
        .connect()
        .await
        .context("failed to connect test ACP client")?;
    Ok(client)
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
