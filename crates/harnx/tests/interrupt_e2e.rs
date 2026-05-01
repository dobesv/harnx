//! End-to-end tests that characterise interrupt handling (Ctrl-C, SIGINT,
//! and ACP `session/cancel`) across TUI, one-shot, and ACP-server modes.
//!
//! Unix-only: these tests rely on SIGINT delivery via `libc::kill` and
//! `tmux` (which is not available in our Windows CI image). The whole
//! module is gated so `cargo test` on Windows still links cleanly.

#![cfg(unix)]

use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

use harnx::test_utils::interrupt::{
    script_call_sub_agent, script_call_trivial_tool, script_call_wait_tool, script_stall_streaming,
    script_streaming_with_sentinel, send_sigint, spawn_acp_client, spawn_oneshot,
    spawn_oneshot_in_tmux, spawn_tui, wait_for_cmd_exit, wait_for_exit, wait_for_prompt_return,
    write_acp_agent, write_minimal_config, write_with_blocking_hook, write_with_sub_agent,
    write_with_wait_tool,
};
use harnx::test_utils::mock_openai_server::MockOpenAiServer;
use harnx::test_utils::tmux_harness::TmuxHarness;
use tokio::time::{timeout, Duration as TokioDuration};

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

/// Regression for issue #358: Ctrl-C in the parent TUI must propagate
/// `session/cancel` to a sub-agent that is actively streaming, otherwise
/// the child's late chunks leak through `AcpNotificationClient`'s
/// fallback path and continue to render in the parent transcript after
/// the abort message.
///
/// The child mock emits `tick-first` immediately and `SENTINEL_END` after
/// 1.5s. We Ctrl-C right after `tick-first` arrives. With the bug, the
/// child keeps running, the mock writes `SENTINEL_END`, and the chunk
/// reaches the parent transcript via the fallback emit. With the fix,
/// the child receives ACP cancel, drops its mock stream, and the sentinel
/// is never written.
#[test]
fn interrupt_tui_sub_agent_cancel_stops_late_chunks() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!("tmux unavailable; skipping interrupt_tui_sub_agent_cancel_stops_late_chunks");
        return Ok(());
    }

    let parent_mock = MockOpenAiServer::start(script_call_sub_agent("child"))?;
    let child_mock = MockOpenAiServer::start(script_streaming_with_sentinel())?;
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
    // Confirm parent delegated and child started streaming.
    tmux.wait_for_contains("Delegating", Duration::from_secs(10))?;
    tmux.wait_for_contains("tick-first", Duration::from_secs(5))?;

    tmux.send_keys(&["C-c"])?;
    wait_for_prompt_return(&tmux, Duration::from_secs(2))?;

    // Wait past the child mock's 1.5s sentinel deadline plus margin. If
    // the child was actually cancelled, no further chunks reach the
    // parent. If cancel propagation is broken, `SENTINEL_END` lands in
    // the transcript via AcpNotificationClient's fallback emit path.
    std::thread::sleep(Duration::from_millis(2500));

    let screen = tmux.capture_pane()?;
    assert!(
        !screen.contains("SENTINEL_END"),
        "SENTINEL_END appeared in transcript after Ctrl-C — sub-agent was not cancelled.\nTranscript:\n{screen}"
    );
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

#[test]
#[cfg(unix)]
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
    // macOS CI runners are noticeably slower at process spawn / ACP handshake
    // than Linux; a 2500 ms budget was occasionally undershooting there,
    // causing the parent to exit normally before we SIGINT'd it. 4000 ms gives
    // a comfortable margin without making the happy-path test meaningfully
    // longer (we only wait the full budget if the spawn is slow).
    std::thread::sleep(Duration::from_millis(4000));

    send_sigint(&child)?;

    let status = wait_for_exit(&mut child, Duration::from_secs(2))?;
    assert!(!status.success(), "expected non-zero exit after SIGINT");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
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
async fn interrupt_acp_session_cancel_during_tool() -> Result<()> {
    let mock = MockOpenAiServer::start(script_call_wait_tool(30))?;
    let tmp = tempfile::tempdir()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mcp_time_bin = harnx::test_utils::interrupt::harnx_mcp_time_bin(&harnx_bin);
    let paths = write_with_wait_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    write_acp_agent(&paths, "default")?;

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
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mcp_time_bin = harnx::test_utils::interrupt::harnx_mcp_time_bin(&harnx_bin);
    let paths = write_with_blocking_hook(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &mcp_time_bin,
    )?;
    write_acp_agent(&paths, "default")?;

    let client = spawn_acp_client(&paths, &harnx_bin, "default").await?;
    let session = client.session_new().await?;

    let prompt_fut = client.session_prompt(Some(&session), "call tool");
    tokio::pin!(prompt_fut);

    // Note: ACP's `eval_tool_calls_async` does NOT currently dispatch
    // PreToolUse hooks (see src/acp/server.rs:661), so the `hook_fired`
    // sentinel used by the TUI / one-shot tests never appears here.
    // We instead wait long enough for the blocking `time_wait` tool to
    // be in flight, then send the cancel. If ACP gains hook support
    // later, swap this sleep for sentinel polling to match the other
    // hook tests.
    tokio::time::sleep(TokioDuration::from_millis(1000)).await;

    client.session_cancel(&session).await?;

    let result = timeout(TokioDuration::from_secs(2), &mut prompt_fut).await;
    assert!(
        result.is_ok(),
        "prompt did not resolve within 2s after cancel"
    );
    Ok(())
}

/// Cancel sent to the top-level ACP server must propagate `session/cancel`
/// down to its sub-agent so the sub-agent stops reading from its upstream.
///
/// Why the assertion uses `child_mock.chunks_written()` rather than the
/// test client's `response_text`: the parent ACP server clears its
/// agent-event sink as part of returning `Cancelled`, so any leaked
/// chunks the sub-agent forwards arrive at a `None` sink and never reach
/// the test client. The child mock's chunk counter, with its
/// peer-closed peek before every write, is what catches the leak — if
/// the child's ACP-side cancel never fires, its connection to the mock
/// stays open past the 1.5 s chunk delay and the mock writes a second
/// chunk; if cancel propagates correctly, the child closes the
/// connection and the second chunk write is skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn interrupt_acp_session_cancel_propagates_to_sub_agent() -> Result<()> {
    let parent_mock = MockOpenAiServer::start(script_call_sub_agent("child"))?;
    let child_mock = MockOpenAiServer::start(script_streaming_with_sentinel())?;
    let tmp = tempfile::tempdir()?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let paths = write_with_sub_agent(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", parent_mock.port()),
        &format!("http://127.0.0.1:{}/v1", child_mock.port()),
        &harnx_bin,
    )?;
    write_acp_agent(&paths, "default")?;

    let client = std::sync::Arc::new(spawn_acp_client(&paths, &harnx_bin, "default").await?);
    let session = client.session_new().await?;

    // Spawn the prompt on its own task so the runtime polls it while
    // we sleep. The previous `tokio::pin!(prompt_fut) + sleep` shape
    // never actually dispatched the prompt during the sleep — the
    // cancel-notify permit then fired immediately on the next prompt's
    // first `.notified()` poll and aborted it before any chunks
    // accumulated, masking the bug.
    let prompt_client = std::sync::Arc::clone(&client);
    let prompt_session = session.clone();
    let prompt_handle = tokio::spawn(async move {
        prompt_client
            .session_prompt(Some(&prompt_session), "delegate please")
            .await
    });

    // Wait long enough for "tick-first" to flow through the chain
    // (parent delegates → child harnx subprocess starts up → child
    // receives prompt → child streams first chunk). Subprocess startup
    // + ACP handshake is the slow link (~1 s on CI). The 1.5 s sentinel
    // deadline inside the child mock starts when the child opens its
    // mock connection, so cancelling at 1.2 s — before the child has
    // advanced to its second chunk — is the right window for catching
    // missed cancel propagation.
    tokio::time::sleep(TokioDuration::from_millis(1200)).await;

    client.session_cancel(&session).await?;

    let _response = timeout(TokioDuration::from_secs(5), prompt_handle)
        .await
        .expect("parent prompt task should resolve within 5 s after cancel")
        .expect("prompt task should not panic")?;

    // Wait past the child mock's 1.5 s sentinel deadline before
    // reading the counter. With cancel propagating, the child closes
    // its mock connection inside the 100 ms HarnxAgent grace and the
    // mock skips the second write; without propagation, the mock
    // bumps chunks_written to 2.
    tokio::time::sleep(TokioDuration::from_millis(2500)).await;

    let chunks = child_mock.chunks_written();
    assert!(
        chunks <= 1,
        "child mock wrote {chunks} chunks; expected at most 1 (tick-first). Cancel did not propagate to the sub-agent."
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
