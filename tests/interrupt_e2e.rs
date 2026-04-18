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
    script_call_wait_tool, script_stall_streaming, spawn_tui, wait_for_prompt_return,
    write_minimal_config, write_with_wait_tool,
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

    tmux.send_text("run wait tool")?;
    tmux.send_keys(&["Enter"])?;
    // Wait until the tool call is visible — the activity log shows the tool name.
    tmux.wait_for_contains("wait", Duration::from_secs(5))?;

    tmux.send_keys(&["C-c"])?;

    wait_for_prompt_return(&tmux, Duration::from_secs(2))?;
    Ok(())
}
