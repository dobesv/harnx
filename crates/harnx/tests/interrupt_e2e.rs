//! End-to-end tests that characterise interrupt handling (Ctrl-C, SIGINT,
//! across TUI and one-shot modes.
//!
//! Unix-only: these tests rely on SIGINT delivery via `libc::kill` and
//! `tmux` (which is not available in our Windows CI image). The whole
//! module is gated so `cargo test` on Windows still links cleanly.

#![cfg(unix)]

use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

use harnx::test_utils::interrupt::{
    script_call_trivial_tool, script_call_wait_tool, script_stall_streaming, send_sigint,
    spawn_oneshot, spawn_oneshot_final_only, spawn_oneshot_in_tmux, spawn_tui, wait_for_cmd_exit,
    wait_for_exit, wait_for_prompt_return, write_minimal_config, write_with_blocking_hook,
    write_with_wait_tool,
};
use harnx::test_utils::mock_openai_server::{
    MockOpenAiError, MockOpenAiScript, MockOpenAiServer, MockOpenAiTurn,
};
use harnx::test_utils::tmux_harness::TmuxHarness;

fn wait_for_mock_request(mock: &MockOpenAiServer) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while mock.get_request_log().is_empty() {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("harnx did not start an LLM request within 10s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn one_shot_local_turn_runs_over_nats() -> Result<()> {
    harnx_core::require_nextest();
    if std::process::Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("nats-server unavailable; skipping one_shot_local_turn_runs_over_nats");
        return Ok(());
    }

    let mock = MockOpenAiServer::start(MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["NATS one-shot smoke".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    })?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mut child = spawn_oneshot(&paths, &harnx_bin, "smoke over local NATS")?;
    let status = wait_for_exit(&mut child, Duration::from_secs(30))?;
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        use std::io::Read;
        pipe.read_to_string(&mut stdout)?;
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        pipe.read_to_string(&mut stderr)?;
    }
    assert!(status.success(), "one-shot exited with {status}");
    assert_eq!(
        stdout.matches("NATS one-shot smoke").count(),
        1,
        "response should be rendered exactly once: {stdout:?}"
    );
    assert!(
        stderr.contains("Resume this session by running:")
            && stderr.contains("harnx -a default -s "),
        "one-shot should report its resumable session: {stderr:?}"
    );
    Ok(())
}

#[test]
fn final_only_prints_only_the_durable_response() -> Result<()> {
    harnx_core::require_nextest();
    if std::process::Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("nats-server unavailable; skipping final_only_prints_only_the_durable_response");
        return Ok(());
    }

    let mock = MockOpenAiServer::start(MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["Only the final answer.".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    })?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mut child = spawn_oneshot_final_only(&paths, &harnx_bin, "answer quietly")?;
    let status = wait_for_exit(&mut child, Duration::from_secs(30))?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        use std::io::Read;
        pipe.read_to_string(&mut stdout)?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        pipe.read_to_string(&mut stderr)?;
    }

    assert!(status.success(), "final-only one-shot exited with {status}");
    assert_eq!(stdout, "Only the final answer.\n");
    assert_eq!(stderr, "");
    Ok(())
}

#[test]
fn final_only_reports_terminal_failures_on_stderr() -> Result<()> {
    harnx_core::require_nextest();
    if std::process::Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!(
            "nats-server unavailable; skipping final_only_reports_terminal_failures_on_stderr"
        );
        return Ok(());
    }

    let mock = MockOpenAiServer::start(MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            error: Some(MockOpenAiError {
                status: 400,
                message: "final-only terminal failure".to_string(),
                error_type: "invalid_request_error".to_string(),
                headers: Vec::new(),
            }),
            ..Default::default()
        }],
        ..Default::default()
    })?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mut child = spawn_oneshot_final_only(&paths, &harnx_bin, "fail quietly")?;
    let status = wait_for_exit(&mut child, Duration::from_secs(30))?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        use std::io::Read;
        pipe.read_to_string(&mut stdout)?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        pipe.read_to_string(&mut stderr)?;
    }

    assert!(
        !status.success(),
        "final-only failure unexpectedly succeeded"
    );
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("final-only terminal failure"),
        "terminal failure should remain diagnosable: {stderr:?}"
    );
    Ok(())
}

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
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mcp_time_bin = harnx::test_utils::interrupt::harnx_mcp_time_bin(&harnx_bin);
    let paths = write_with_wait_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
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
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mcp_time_bin = harnx::test_utils::interrupt::harnx_mcp_time_bin(&harnx_bin);
    let paths = write_with_blocking_hook(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmux = spawn_tui(&paths, &harnx_bin, &repo_root)?;

    tmux.send_text("go")?;
    tmux.send_keys(&["Enter"])?;
    // Wait for the LLM's streamed text — proves the LLM has responded
    // and harnx is about to dispatch the tool, which triggers the hook.
    tmux.wait_for_contains("Listing", Duration::from_secs(5))?;
    // Poll for the hook's sentinel so the test doesn't race the hook
    // subprocess on slow CI runners.
    let sentinel = paths.dir.join("hook_fired");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !sentinel.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("PreToolUse hook never fired (sentinel missing after 10s)");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    tmux.send_keys(&["C-c"])?;

    wait_for_prompt_return(&tmux, Duration::from_secs(2))?;
    Ok(())
}
#[test]
#[cfg(unix)]
fn interrupt_oneshot_during_streaming() -> Result<()> {
    let mock = MockOpenAiServer::start(script_stall_streaming())?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mut child = spawn_oneshot(&paths, &harnx_bin, "hello")?;

    wait_for_mock_request(&mock)?;

    send_sigint(&child)?;

    let status = wait_for_exit(&mut child, Duration::from_secs(2))?;
    assert!(!status.success(), "expected non-zero exit after SIGINT");
    Ok(())
}

#[test]
#[cfg(unix)]
fn interrupt_oneshot_during_tool() -> Result<()> {
    let mock = MockOpenAiServer::start(script_call_wait_tool(30))?;
    let tmp = tempfile::tempdir()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mcp_time_bin = harnx::test_utils::interrupt::harnx_mcp_time_bin(&harnx_bin);
    let paths = write_with_wait_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    let mut child = spawn_oneshot(&paths, &harnx_bin, "wait please")?;

    // Allow the LLM round-trip + tool dispatch (~1s in practice).
    std::thread::sleep(Duration::from_millis(1500));

    send_sigint(&child)?;

    let status = wait_for_exit(&mut child, Duration::from_secs(2))?;
    assert!(!status.success(), "expected non-zero exit after SIGINT");
    Ok(())
}

#[test]
#[cfg(unix)]
fn interrupt_oneshot_during_hook() -> Result<()> {
    let mock = MockOpenAiServer::start(script_call_trivial_tool())?;
    let tmp = tempfile::tempdir()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mcp_time_bin = harnx::test_utils::interrupt::harnx_mcp_time_bin(&harnx_bin);
    let paths = write_with_blocking_hook(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    let mut child = spawn_oneshot(&paths, &harnx_bin, "call a tool")?;

    // Poll for the hook's sentinel rather than a fixed sleep — CI runners
    // are slower than local for harnx startup + LLM round-trip + hook
    // spawn. Once the sentinel exists we know block.sh is actively
    // sleeping, so it's safe to deliver SIGINT and assert cancellation.
    let sentinel = paths.dir.join("hook_fired");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !sentinel.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("PreToolUse hook never fired (sentinel missing after 10s)");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    send_sigint(&child)?;

    let status = wait_for_exit(&mut child, Duration::from_secs(2))?;
    assert!(!status.success(), "expected non-zero exit after SIGINT");
    Ok(())
}
/// Verify that Ctrl-C cancels a one-shot (Cmd) prompt while streaming output
/// is in progress and crossterm raw mode is active.
///
/// Unlike the existing `interrupt_oneshot_during_streaming` test (which sends
/// `SIGINT` directly via `kill(2)`, bypassing the terminal), this test runs
/// harnx inside a real tmux pane so that Ctrl-C is delivered as a terminal key
/// event through crossterm's event stream — exactly as a real user would
/// experience it.  If the raw-mode key watcher (`spawn_raw_mode_key_watcher`)
/// is missing or broken, Ctrl-C is swallowed and this test times out.
#[test]
fn interrupt_cmd_raw_mode_ctrlc() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!("tmux unavailable; skipping interrupt_cmd_raw_mode_ctrlc");
        return Ok(());
    }

    let mock = MockOpenAiServer::start(script_stall_streaming())?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmux = spawn_oneshot_in_tmux(&paths, &harnx_bin, "hello", &repo_root)?;

    // Wait until at least the first streaming chunk ("Thinking") is visible
    // in the pane — this means harnx has received data from the mock LLM and
    // crossterm raw mode is active inside CliAgentEventSink.
    tmux.wait_for_contains("Thinking", Duration::from_secs(10))?;

    // Send Ctrl-C as a real terminal key event (not SIGINT).  In raw mode
    // this is the only reliable way to interrupt the process.
    tmux.send_keys(&["C-c"])?;

    // harnx should exit non-zero quickly.  On success the shell prints
    // "HARNX_EXIT:<code>"; wait_for_cmd_exit polls for that sentinel.
    let nonzero = wait_for_cmd_exit(&tmux, Duration::from_secs(5))?;
    assert!(
        nonzero,
        "expected non-zero exit after Ctrl-C in raw-mode streaming"
    );
    Ok(())
}
