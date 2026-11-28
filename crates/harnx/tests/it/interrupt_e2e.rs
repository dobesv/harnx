//! End-to-end tests that characterise interrupt handling (Ctrl-C, SIGINT,
//! across TUI and one-shot modes.
//!
//! Unix-only: these tests rely on SIGINT delivery via `libc::kill` and
//! `tmux` (which is not available in our Windows CI image). The whole
//! module is gated so `cargo test` on Windows still links cleanly.

#![cfg(unix)]

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use harnx::test_utils::interrupt::{
    assert_process_reaped, script_call_trivial_tool, script_call_wait_tool, script_stall_streaming,
    send_sigint, spawn_oneshot, spawn_oneshot_final_only, spawn_oneshot_in_tmux,
    spawn_oneshot_with_args, spawn_tui, wait_for_cmd_exit, wait_for_exit, wait_for_prompt_return,
    write_minimal_config, write_with_blocking_hook, write_with_wait_tool, GatedNats,
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

fn collect_child_output(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String, String)> {
    use std::io::Read;

    let status = wait_for_exit(child, timeout)?;
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)?;
    }
    Ok((status, stdout, stderr))
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
fn one_shot_timeout_emits_contract_then_same_session_retry_succeeds() -> Result<()> {
    harnx_core::require_nextest();
    if std::process::Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!(
            "nats-server unavailable; skipping one_shot_timeout_emits_contract_then_same_session_retry_succeeds"
        );
        return Ok(());
    }

    let mock = MockOpenAiServer::start(MockOpenAiScript {
        turns: vec![MockOpenAiTurn {
            text_chunks: vec!["partial".to_string(), "too late".to_string()],
            ..Default::default()
        }],
        fallback_text: "same-session retry succeeded".to_string(),
        chunk_delay_ms: 2_000,
    })?;
    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), &format!("http://127.0.0.1:{}/v1", mock.port()))?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let mut child = spawn_oneshot_with_args(
        &paths,
        &harnx_bin,
        "time out this turn",
        &["--final-only", "--timeout-secs", "1"],
    )?;
    let (status, stdout, stderr) = collect_child_output(&mut child, Duration::from_secs(10))?;

    assert_eq!(status.code(), Some(2), "stderr: {stderr:?}");
    assert!(stdout.contains("reaching its time limit"), "{stdout:?}");
    assert!(
        stdout.contains("same session id"),
        "missing same-session retry hint: {stdout:?}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr:?}");
    let termination: serde_json::Value = serde_json::from_str(stderr.trim())?;
    assert_eq!(termination["kind"], "timeout");
    let session_id = termination["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .expect("timeout contract should contain a session id")
        .to_string();

    // Acceptance need not wait for worker startup or transcript projection.
    // If the deadline won before model dispatch, G2 consumes the first mock turn.
    let calls_before_retry = mock.get_request_log().len();
    let mut retry = spawn_oneshot_with_args(
        &paths,
        &harnx_bin,
        "retry after timeout",
        &["--final-only", "--session", &session_id],
    )?;
    // The nextest sandbox proves the fix only where the runtime can build a
    // strong host identity (hashed machine/boot/pid-namespace) and reclaim
    // the dead holder's lease immediately (see
    // `nats_lease::holder_reclaim_supported`), so the retry must finish well
    // inside the 30 s lease TTL. Platforms that keep the TTL-only reclaim
    // (macOS, Windows) retain the old tolerant budget.
    let retry_budget_secs = if harnx_runtime::nats_lease::holder_reclaim_supported() {
        20
    } else {
        45
    };
    let (retry_status, retry_stdout, retry_stderr) =
        collect_child_output(&mut retry, Duration::from_secs(retry_budget_secs))?;
    assert!(
        retry_status.success(),
        "same-session retry exited with {retry_status}; stderr: {retry_stderr:?}"
    );
    assert_eq!(retry_stderr, "");
    assert_retry_response(&mock, calls_before_retry, &retry_stdout);
    Ok(())
}

fn assert_retry_response(mock: &MockOpenAiServer, calls_before_retry: usize, retry_stdout: &str) {
    let expected = if calls_before_retry == 0 {
        "partialtoo late\n"
    } else {
        "same-session retry succeeded\n"
    };
    assert_eq!(retry_stdout, expected);
    let requests = mock.get_request_log();
    assert_eq!(requests.len(), calls_before_retry + 1);
    let messages = requests.last().unwrap()["messages"].as_array().unwrap();
    let notice = messages.last().unwrap();
    assert_eq!(notice["role"], "user");
    assert!(harnx_core::timing_identity::is_timing_notice(
        notice["content"].as_str().expect("timing notice text")
    ));
    assert_eq!(
        messages[messages.len() - 2]["content"],
        "retry after timeout"
    );
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
        "final-only failure unexpectedly succeeded: stdout={stdout:?} stderr={stderr:?} log={:?}",
        std::fs::read_to_string(paths.harnx_state_dir.join("harnx.log"))
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
    let time_tools_bin = harnx::test_utils::interrupt::harnx_time_tools_bin(&harnx_bin);
    let paths = write_with_wait_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &time_tools_bin,
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
    let time_tools_bin = harnx::test_utils::interrupt::harnx_time_tools_bin(&harnx_bin);
    let paths = write_with_blocking_hook(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &time_tools_bin,
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
            panic!("Blocking hook never fired (sentinel missing after 10s)");
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
    let time_tools_bin = harnx::test_utils::interrupt::harnx_time_tools_bin(&harnx_bin);
    let paths = write_with_wait_tool(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &time_tools_bin,
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
    let time_tools_bin = harnx::test_utils::interrupt::harnx_time_tools_bin(&harnx_bin);
    let paths = write_with_blocking_hook(
        tmp.path(),
        &format!("http://127.0.0.1:{}/v1", mock.port()),
        &time_tools_bin,
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
            panic!("Blocking hook never fired (sentinel missing after 10s)");
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

/// Broker-wait notice cadence seam for the managed-startup tests below, same
/// value as `patient_nats_cli::NOTICE_SEAM_MS`.
const NOTICE_SEAM_MS: &str = "200";

/// Managed-local broker acquisition during TUI bootstrap blocks before the
/// terminal is set up (design 8): the wait notice stays on stderr — the TUI
/// event sink is not installed yet — and Ctrl-C exits through cleanup, not a
/// raw SIGINT death. `run_main` awaits the broker scope shutdown on the way
/// out, so the owned broker child is reaped and a second TUI can start once
/// the gate opens.
///
/// Like `interrupt_cmd_raw_mode_ctrlc`, this drives a real tmux pane so Ctrl-C
/// is delivered as the terminal would deliver it, before raw mode exists.
#[test]
fn nats_tui_ctrl_c_before_terminal_setup_exits_through_cleanup() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!("tmux unavailable; skipping nats_tui_ctrl_c_before_terminal_setup_exits_through_cleanup");
        return Ok(());
    }
    let Some(gate) = GatedNats::new()? else {
        eprintln!("nats-server unavailable; skipping nats_tui_ctrl_c_before_terminal_setup_exits_through_cleanup");
        return Ok(());
    };

    let tmp = tempfile::tempdir()?;
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let envs = gate.pane_env(NOTICE_SEAM_MS);

    let tmux = TmuxHarness::new(&repo_root, 120, 35)?;
    let mut export_line = format!(
        "export HARNX_CONFIG_DIR={} HARNX_DATA_DIR={} HARNX_STATE_DIR={}\n",
        paths.harnx_config_dir.to_string_lossy(),
        paths.harnx_data_dir.to_string_lossy(),
        paths.harnx_state_dir.to_string_lossy()
    );
    for (key, value) in &envs {
        export_line = format!("{export_line}export {key}={value}\n");
    }
    tmux.send_text(&export_line)?;
    tmux.send_text(&format!(
        "{} --agent default --session || echo HARNX_EXIT:$?\n",
        harnx_bin.to_string_lossy()
    ))?;

    // Bootstrap blocks on the gated broker; the notice proves the stderr
    // route is live before the terminal is set up.
    tmux.wait_for_contains("Waiting for nats-server", Duration::from_secs(20))?;
    let broker_pid = gate.started(0)?;

    tmux.send_keys(&["C-c"])?;
    tmux.wait_for_contains("HARNX_EXIT:1", Duration::from_secs(10))?;
    // The owned broker child was reaped before the process exited, so the
    // OS-level lock it held is free for the next process.
    assert_process_reaped(broker_pid);

    // With the gate open, a fresh TUI acquires the broker and reaches the
    // rendered input area — the scope shutdown of the cancelled run really
    // released the broker. Same readiness predicate as `spawn_tui`.
    gate.allow()?;
    tmux.send_text(&format!(
        "{} --agent default --session\n",
        harnx_bin.to_string_lossy()
    ))?;
    let spinner_frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    tmux.wait_for(Duration::from_secs(20), |screen| {
        screen.contains("Input")
            || screen.contains("• 🤖")
            || spinner_frames.iter().any(|frame| screen.contains(*frame))
    })?;
    Ok(())
}

// libc::kill has no safe wrapper; the PID belongs to this test's gated
// broker fixture.
fn sigkill(pid: u32) {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    assert_eq!(
        result,
        0,
        "failed to kill the broker child: {}",
        std::io::Error::last_os_error()
    );
}

/// Active-TUI evidence for the patient managed-local broker (plan #2922, T2):
/// with the TUI fully interactive, kill the owned broker and close the gate so
/// broker recovery is pending. While a broker-waiting history command (the
/// `.empty session` clear) waits on recovery, the input loop must stay
/// responsive (typed text renders), broker wait notices reach the transcript,
/// and Ctrl-C cancels the pending command — its cancellation error lands in
/// the transcript and the TUI stays alive. Reopening the gate completes the
/// recovery and the resubmitted command finishes (its mutation schedules the
/// history rebuild); a clean Ctrl-D exit then goes through the broker scope
/// shutdown, reaping the recovered broker child.
#[test]
fn nats_tui_active_cancels_broker_recovery_and_recovers() -> Result<()> {
    if !TmuxHarness::is_available() {
        eprintln!(
            "tmux unavailable; skipping nats_tui_active_cancels_broker_recovery_and_recovers"
        );
        return Ok(());
    }
    let Some(gate) = GatedNats::new()? else {
        eprintln!("nats-server unavailable; skipping nats_tui_active_cancels_broker_recovery_and_recovers");
        return Ok(());
    };
    // The gate is open from the start: the TUI must come up fully interactive
    // against a ready broker before the broker is killed.
    gate.allow()?;
    let tmp = tempfile::tempdir()?;
    // No model is called in this test; the config only needs the agent.
    let paths = write_minimal_config(tmp.path(), "http://127.0.0.1:1/v1")?;
    let harnx_bin = PathBuf::from(env!("CARGO_BIN_EXE_harnx"));
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let envs = gate.pane_env(NOTICE_SEAM_MS);

    let tmux = TmuxHarness::new(&repo_root, 120, 35)?;
    let mut export_line = format!(
        "export HARNX_CONFIG_DIR={} HARNX_DATA_DIR={} HARNX_STATE_DIR={}\n",
        paths.harnx_config_dir.to_string_lossy(),
        paths.harnx_data_dir.to_string_lossy(),
        paths.harnx_state_dir.to_string_lossy()
    );
    for (key, value) in &envs {
        export_line = format!("{export_line}export {key}={value}\n");
    }
    tmux.send_text(&export_line)?;
    // `; echo HARNX_EXIT:$?` reports both success and failure exits.
    tmux.send_text(&format!(
        "{} --agent default --session; echo HARNX_EXIT:$?\n",
        harnx_bin.to_string_lossy()
    ))?;
    let spinner_frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    tmux.wait_for(Duration::from_secs(20), |screen| {
        screen.contains("Input")
            || screen.contains("• 🤖")
            || spinner_frames.iter().any(|frame| screen.contains(*frame))
    })
    .context("TUI did not reach the interactive input area")?;
    let broker_pid = gate.started(0)?;

    // Pending recovery: kill the owned broker child and close the gate, so
    // the frontend's restarted broker cannot report its port.
    gate.close()?;
    sigkill(broker_pid);

    // A broker-waiting history command: `.empty session` clears the remote
    // session log as cancellable background work (design 9). Its broker wait
    // reports through the rerouted transcript notice.
    tmux.send_text(".empty session")?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for_contains("Waiting for nats-server", Duration::from_secs(10))?;
    // Input loop responsiveness during the pending recovery: typed text
    // renders in the input area while the broker is unavailable.
    tmux.send_text("still-alive")?;
    tmux.wait_for_contains("still-alive", Duration::from_secs(5))
        .context("input loop froze while broker recovery was pending")?;

    // Ctrl-C cancels the pending command: the cancellation error lands in the
    // transcript and the TUI stays alive (no exit sentinel appears).
    tmux.send_keys(&["C-c"])?;
    // The cancellation error rendering after Ctrl-C proves the event loop
    // is still alive, and the TUI must not have exited.
    let screen = tmux
        .wait_for_contains("cancelled while waiting for NATS", Duration::from_secs(10))
        .context("Ctrl-C must cancel the pending history command")?;
    assert!(
        !screen.contains("HARNX_EXIT:"),
        "the TUI must stay alive after Ctrl-C cancelled the pending command"
    );

    // Recovery: reopening the gate lets the restarted broker report its
    // port. The resubmitted clear then completes. Its rebuild's "Loading
    // session history…" status line is transient (the rebuild replaces the
    // transcript within one capture poll), so completion is proven with a
    // second history command queued behind the clear: `.session` with one
    // argument fails deterministically once the queue drains past the clear,
    // and its error text stays in the transcript.
    gate.allow()?;
    // Clear the probe draft, then resubmit.
    for _ in 0.."still-alive".len() {
        tmux.send_keys(&["BSpace"])?;
    }
    tmux.send_text(".empty session")?;
    tmux.send_keys(&["Enter"])?;
    tmux.send_text(".session one-arg")?;
    tmux.send_keys(&["Enter"])?;
    tmux.wait_for_contains(
        "An explicit agent and session ID are required",
        Duration::from_secs(30),
    )
    .context("frontend did not recover after the broker returned")?;

    // Clean shutdown: exit with Ctrl-D. The frontend scope awaits the broker
    // shutdown, so the recovered child is reaped.
    tmux.send_keys(&["C-d"])?;
    tmux.wait_for_contains("HARNX_EXIT:0", Duration::from_secs(30))?;
    let recovered_pid = gate.started(1)?;
    assert_process_reaped(recovered_pid);
    Ok(())
}
