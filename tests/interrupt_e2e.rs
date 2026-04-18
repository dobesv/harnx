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
    send_sigint, spawn_oneshot, spawn_tui, wait_for_exit, wait_for_prompt_return,
    write_minimal_config, write_with_blocking_hook, write_with_sub_agent, write_with_wait_tool,
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
    // "Delegating..." proves the parent LLM responded.
    tmux.wait_for_contains("Delegating", Duration::from_secs(10))?;
    // "Thinking" only comes from the child's mock — proves the ACP
    // delegation reached the child and the child started streaming.
    tmux.wait_for_contains("Thinking", Duration::from_secs(5))?;

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

    // Give harnx time to make the LLM call and start streaming.
    std::thread::sleep(Duration::from_millis(500));

    send_sigint(&child)?;

    let status = wait_for_exit(&mut child, Duration::from_secs(2))?;
    assert!(!status.success(), "expected non-zero exit after SIGINT");
    Ok(())
}

#[test]
#[cfg(unix)]
#[ignore = "pending interrupt fix (#292): SIGINT during tool exits zero"]
fn interrupt_oneshot_during_tool() -> Result<()> {
    let mock = MockOpenAiServer::start(script_call_wait_tool(30))?;
    let tmp = tempfile::tempdir()?;
    let mcp_time_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx-mcp-time"));
    let paths = write_with_wait_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
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
    let mcp_time_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx-mcp-time"));
    let paths = write_with_blocking_hook(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mut child = spawn_oneshot(&paths, &harnx_bin, "call a tool")?;

    // Allow LLM response + hook to start (sleep 30 in block.sh).
    std::thread::sleep(Duration::from_millis(1500));

    // Hook should have fired by now.
    assert!(
        paths.dir.join("hook_fired").exists(),
        "PreToolUse hook never fired (sentinel missing)"
    );

    send_sigint(&child)?;

    let status = wait_for_exit(&mut child, Duration::from_secs(2))?;
    assert!(!status.success(), "expected non-zero exit after SIGINT");
    Ok(())
}

#[test]
#[cfg(unix)]
#[ignore = "pending interrupt fix (#292): SIGINT during sub-agent exits zero"]
fn interrupt_oneshot_during_sub_agent() -> Result<()> {
    let parent_mock = MockOpenAiServer::start(script_call_sub_agent("child"))?;
    let child_mock = MockOpenAiServer::start(script_stall_streaming())?;
    let tmp = tempfile::tempdir()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let paths = write_with_sub_agent(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", parent_mock.port()),
        &format!("http://127.0.0.1:{}/v1", child_mock.port()),
        &harnx_bin,
    )?;
    let mut child = spawn_oneshot(&paths, &harnx_bin, "delegate")?;

    // Allow parent LLM + ACP handshake + child startup + child streaming.
    std::thread::sleep(Duration::from_millis(2500));

    send_sigint(&child)?;

    let status = wait_for_exit(&mut child, Duration::from_secs(2))?;
    assert!(!status.success(), "expected non-zero exit after SIGINT");
    Ok(())
}

use harnx::test_utils::interrupt::{spawn_acp_client, write_acp_agent};
use tokio::time::{timeout, Duration as TokioDuration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
#[ignore = "pending interrupt fix (#292): session/cancel during streaming hangs"]
async fn interrupt_acp_session_cancel_during_streaming() -> Result<()> {
    let mock = MockOpenAiServer::start(script_stall_streaming())?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    write_acp_agent(&paths, "default")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let client = spawn_acp_client(&paths, &harnx_bin, "default").await?;
    let session = client.session_new().await?;

    let prompt_fut = client.session_prompt(Some(&session), "hello");
    tokio::pin!(prompt_fut);
    tokio::time::sleep(TokioDuration::from_millis(500)).await;

    client.session_cancel(&session).await?;

    let result = timeout(TokioDuration::from_secs(2), &mut prompt_fut).await;
    assert!(
        result.is_ok(),
        "prompt did not resolve within 2s after cancel"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
#[ignore = "pending interrupt fix (#292): session/cancel during tool hangs"]
async fn interrupt_acp_session_cancel_during_tool() -> Result<()> {
    let mock = MockOpenAiServer::start(script_call_wait_tool(30))?;
    let tmp = tempfile::tempdir()?;
    let mcp_time_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx-mcp-time"));
    let paths = write_with_wait_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    write_acp_agent(&paths, "default")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let client = spawn_acp_client(&paths, &harnx_bin, "default").await?;
    let session = client.session_new().await?;

    let prompt_fut = client.session_prompt(Some(&session), "wait please");
    tokio::pin!(prompt_fut);
    tokio::time::sleep(TokioDuration::from_millis(1000)).await;

    client.session_cancel(&session).await?;

    let result = timeout(TokioDuration::from_secs(2), &mut prompt_fut).await;
    assert!(
        result.is_ok(),
        "prompt did not resolve within 2s after cancel"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn interrupt_acp_session_cancel_during_hook() -> Result<()> {
    let mock = MockOpenAiServer::start(script_call_trivial_tool())?;
    let tmp = tempfile::tempdir()?;
    let mcp_time_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx-mcp-time"));
    let paths = write_with_blocking_hook(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    write_acp_agent(&paths, "default")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    let client = spawn_acp_client(&paths, &harnx_bin, "default").await?;
    let session = client.session_new().await?;

    let prompt_fut = client.session_prompt(Some(&session), "call tool");
    tokio::pin!(prompt_fut);
    tokio::time::sleep(TokioDuration::from_millis(1000)).await;

    client.session_cancel(&session).await?;

    let result = timeout(TokioDuration::from_secs(2), &mut prompt_fut).await;
    assert!(
        result.is_ok(),
        "prompt did not resolve within 2s after cancel"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
#[ignore = "pending interrupt fix (#292): session/cancel does not propagate to sub-agent"]
async fn interrupt_acp_session_cancel_propagates_to_sub_agent() -> Result<()> {
    let parent_mock = MockOpenAiServer::start(script_call_sub_agent("child"))?;
    let child_mock = MockOpenAiServer::start(script_stall_streaming())?;
    let tmp = tempfile::tempdir()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let paths = write_with_sub_agent(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", parent_mock.port()),
        &format!("http://127.0.0.1:{}/v1", child_mock.port()),
        &harnx_bin,
    )?;
    write_acp_agent(&paths, "default")?;

    let client = spawn_acp_client(&paths, &harnx_bin, "default").await?;
    let session = client.session_new().await?;

    let prompt_fut = client.session_prompt(Some(&session), "delegate please");
    tokio::pin!(prompt_fut);
    tokio::time::sleep(TokioDuration::from_millis(2000)).await;

    client.session_cancel(&session).await?;

    let result = timeout(TokioDuration::from_secs(2), &mut prompt_fut).await;
    assert!(
        result.is_ok(),
        "parent prompt did not resolve after cancel — sub-agent likely not cancelled"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn interrupt_acp_sigint_cancels_and_exits() -> Result<()> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mock = MockOpenAiServer::start(script_stall_streaming())?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    write_acp_agent(&paths, "default")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));

    // Spawn `harnx --acp default` with raw stdio. We don't need the
    // RPC responses parsed — just need the child to be busy enough
    // that SIGINT is meaningful, and to confirm it then exits.
    let mut child = Command::new(&harnx_bin)
        .arg("--acp")
        .arg("default")
        .env("HARNX_CONFIG_DIR", &paths.harnx_config_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .expect("acp child stdin should be piped");
        // Minimal handshake. The actual session-id / protocol details may
        // mismatch — that's fine; what matters is that the child stays
        // alive reading stdin and processing requests when SIGINT arrives.
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1,"clientCapabilities":{{}}}}}}"#
        )?;
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"/tmp","mcpServers":[]}}}}"#
        )?;
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"test","prompt":[{{"type":"text","text":"hello"}}]}}}}"#
        )?;
    }

    std::thread::sleep(Duration::from_millis(1000));

    send_sigint(&child)?;

    let _status = wait_for_exit(&mut child, Duration::from_secs(2))?;
    Ok(())
}
