//! End-to-end tests that characterise interrupt handling (Ctrl-C, SIGINT,
//! and ACP `session/cancel`) across TUI, one-shot, and ACP-server modes.
//!
//! Tests that currently fail against `main` are marked with
//! `#[ignore = "pending interrupt fix (#292)"]`. Run the full suite with
//!   cargo nextest run --test interrupt_e2e --run-ignored=all
//! to see the pre-fix baseline. As fixes land, individual tests are
//! un-ignored in the same PR.

use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

use harnx::test_utils::interrupt::{
    script_call_sub_agent, script_call_trivial_tool, script_call_wait_tool, script_stall_streaming,
    spawn_tui, wait_for_prompt_return, write_minimal_config, write_with_blocking_hook,
    write_with_sub_agent, write_with_wait_tool,
};
use harnx::test_utils::mock_openai_server::MockOpenAiServer;
use harnx::test_utils::tmux_harness::TmuxHarness;

#[test]
fn interrupt_tui_during_streaming() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!("tmux unavailable; skipping interrupt_tui_during_streaming");
        return Ok(());
    }

    let mock = MockOpenAiServer::start(script_stall_streaming())?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmux = spawn_tui(&paths, &harnx_bin, &repo_root)?;

    tmux.send_text("hello")?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for_contains("Thinking", Duration::from_secs(5))?;

    tmux.send_keys(&["C-c"])?;

    wait_for_prompt_return(&tmux, Duration::from_secs(2))?;
    Ok(())
}

#[test]
fn interrupt_tui_during_tool() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!("tmux unavailable; skipping interrupt_tui_during_tool");
        return Ok(());
    }

    let mock = MockOpenAiServer::start(script_call_wait_tool(30))?;
    let tmp = tempfile::tempdir()?;
    let mcp_time_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx-mcp-time"));
    let paths = write_with_wait_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmux = spawn_tui(&paths, &harnx_bin, &repo_root)?;

    tmux.send_text("go")?;
    tmux.send_keys(&["Enter"])?;
    // Wait for the LLM's text chunk "Waiting..." to render and brief
    // pause so the wait tool actually starts executing in the MCP server
    // (not just the LLM text being on screen).
    tmux.wait_for_contains("Waiting", Duration::from_secs(5))?;
    std::thread::sleep(Duration::from_millis(500));

    tmux.send_keys(&["C-c"])?;

    wait_for_prompt_return(&tmux, Duration::from_secs(2))?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn interrupt_tui_during_hook() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!("tmux unavailable; skipping interrupt_tui_during_hook");
        return Ok(());
    }

    let mock = MockOpenAiServer::start(script_call_trivial_tool())?;
    let tmp = tempfile::tempdir()?;
    let mcp_time_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx-mcp-time"));
    let paths = write_with_blocking_hook(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmux = spawn_tui(&paths, &harnx_bin, &repo_root)?;

    tmux.send_text("go")?;
    tmux.send_keys(&["Enter"])?;
    // Wait for the LLM's streamed text — proves the LLM has responded
    // and harnx is about to dispatch the tool, which triggers the hook.
    tmux.wait_for_contains("Listing", Duration::from_secs(5))?;
    // Pause to ensure the PreToolUse hook (sleep 30) has actually started.
    std::thread::sleep(Duration::from_millis(500));

    // Confirm the hook actually fired before asserting cancellation.
    // (Without this, a false-positive could occur if the hook is silently
    // skipped — the test would pass for the wrong reason.)
    assert!(
        paths.dir.join("hook_fired").exists(),
        "PreToolUse hook never wrote sentinel — hook may not be wired in"
    );

    tmux.send_keys(&["C-c"])?;

    wait_for_prompt_return(&tmux, Duration::from_secs(2))?;
    Ok(())
}

#[test]
fn interrupt_tui_during_sub_agent() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!("tmux unavailable; skipping interrupt_tui_during_sub_agent");
        return Ok(());
    }

    // Two mock LLMs — one for the parent (delegates), one for the child (stalls).
    let parent_mock = MockOpenAiServer::start(script_call_sub_agent("child"))?;
    let child_mock = MockOpenAiServer::start(script_stall_streaming())?;
    let tmp = tempfile::tempdir()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let paths = write_with_sub_agent(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", parent_mock.port()),
        &format!("http://127.0.0.1:{}/v1", child_mock.port()),
        &harnx_bin,
    )?;
    let tmux = spawn_tui(&paths, &harnx_bin, &repo_root)?;

    tmux.send_text("delegate please")?;
    tmux.send_keys(&["Enter"])?;
    // Wait for the parent's "Delegating..." text — proves the parent LLM
    // responded and is about to invoke the sub-agent ACP tool.
    tmux.wait_for_contains("Delegating", Duration::from_secs(10))?;
    // Allow time for the child harnx process to start and begin streaming.
    std::thread::sleep(Duration::from_millis(2000));

    tmux.send_keys(&["C-c"])?;

    wait_for_prompt_return(&tmux, Duration::from_secs(2))?;
    Ok(())
}
