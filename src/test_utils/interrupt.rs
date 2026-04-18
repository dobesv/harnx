//! Helpers for interrupt-handling e2e tests (see `tests/interrupt_e2e.rs`).
//!
//! This module is compiled only under `cfg(test)` via `src/test_utils/mod.rs`.
//! It intentionally does not expose anything outside the crate.

use crate::test_utils::mock_openai_server::{MockOpenAiScript, MockOpenAiTurn};
use crate::test_utils::tmux_harness::TmuxHarness;
use anyhow::{Context, Result};
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
    std::fs::write(&block_sh, "#!/bin/sh\nsleep 30\n").context("failed to write block.sh")?;
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
