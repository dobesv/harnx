use super::*;
use anyhow::{anyhow, Result};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

fn acp_test_binary_path() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_harnx-acp-test") {
        return PathBuf::from(path);
    }

    let current_exe = std::env::current_exe().expect("current test executable path");
    let deps_dir = current_exe
        .parent()
        .expect("test executable should have parent directory");
    let target_dir = deps_dir
        .parent()
        .expect("deps directory should have target profile parent");
    target_dir.join(format!("harnx-acp-test{}", std::env::consts::EXE_SUFFIX))
}

fn cancel_sentinel_path() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("harnx-issue-68-cancel-{unique}.txt"))
}

async fn wait_for_sentinel(path: &Path) -> Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = tokio::fs::read_to_string(path).await {
            return Ok(contents);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("cancel sentinel was not created"));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn spawn_mock_issue_68_server() -> Result<Child> {
    let binary_path = acp_test_binary_path();
    let mut child = Command::new(&binary_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .expect("mock ACP server should have stdout");
    let mut reader = BufReader::new(stdout).lines();
    let ready = tokio::time::timeout(Duration::from_secs(5), reader.next_line())
        .await
        .expect("mock ACP server ready line timeout")?
        .expect("mock ACP server should emit ready line");
    assert_eq!(ready, "READY");
    child.stdout = Some(reader.into_inner().into_inner());
    Ok(child)
}

async fn assert_mock_server_is_reachable(child: &mut Child) -> Result<()> {
    if let Some(status) = child.try_wait()? {
        return Err(anyhow!(
            "mock ACP server exited unexpectedly with status {status}"
        ));
    }
    Ok(())
}

async fn send_sigint_after(delay: Duration) {
    tokio::time::sleep(delay).await;
    unsafe {
        libc::raise(libc::SIGINT);
    }
}

struct SignalTrap {
    previous: libc::sighandler_t,
}

impl SignalTrap {
    fn install() -> Self {
        extern "C" fn handler(_sig: libc::c_int) {}

        let previous =
            unsafe { libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t) };
        Self { previous }
    }
}

impl Drop for SignalTrap {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGINT, self.previous);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_call_tool_session_prompt_ctrl_c_cancels_session() {
    let _signal_trap = SignalTrap::install();
    let sentinel_path = cancel_sentinel_path();
    let _ = std::fs::remove_file(&sentinel_path);

    let mut env = HashMap::new();
    env.insert(
        "ACP_CANCEL_SENTINEL".to_string(),
        sentinel_path.display().to_string(),
    );

    let mut server = spawn_mock_issue_68_server()
        .await
        .expect("spawn mock ACP issue 68 server");

    let manager = AcpManager::new();
    manager.initialize(vec![AcpServerConfig {
        name: "issue68".to_string(),
        command: acp_test_binary_path().display().to_string(),
        args: vec![],
        env,
        enabled: true,
        description: Some("mock ACP server for issue 68 regression".to_string()),
        idle_timeout_secs: 10,
        operation_timeout_secs: 10,
    }]);

    let ctrl_c_task = tokio::spawn(send_sigint_after(Duration::from_millis(300)));
    let result = manager
        .call_tool(
            "issue68_session_prompt",
            json!({ "message": "please hang" }),
        )
        .await;
    ctrl_c_task.await.expect("ctrl-c sender task should finish");

    let err = result.expect_err("ctrl_c should abort ACP session_prompt");
    assert!(err.to_string().contains("aborted by user"));

    let cancelled_session_id = wait_for_sentinel(&sentinel_path)
        .await
        .expect("mock server should record ACP cancellation");
    assert_eq!(cancelled_session_id.trim(), "session-1");

    assert_mock_server_is_reachable(&mut server)
        .await
        .expect("mock server should still be responsive after cancellation");

    if let Some(mut stdin) = server.stdin.take() {
        let _ = stdin.write_all(b"STOP\n").await;
        let _ = stdin.flush().await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), server.wait()).await;
    let _ = std::fs::remove_file(&sentinel_path);
}
