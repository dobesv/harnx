use super::*;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn acp_test_binary_path() -> PathBuf {
    let candidates = [
        std::env::var("NEXTEST_BIN_EXE_harnx-acp-test").ok(),
        std::env::var("CARGO_BIN_EXE_harnx-acp-test").ok(),
    ];

    for candidate in candidates.into_iter().flatten() {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }

    let current_exe = std::env::current_exe().expect("current test executable path");
    let deps_dir = current_exe
        .parent()
        .expect("test executable should have parent directory");
    let target_dir = deps_dir
        .parent()
        .expect("deps directory should have target profile parent");
    let fallback = target_dir.join(format!("harnx-acp-test{}", std::env::consts::EXE_SUFFIX));
    assert!(
        fallback.is_file(),
        "ACP test helper binary not found. Checked NEXTEST_BIN_EXE_harnx-acp-test, CARGO_BIN_EXE_harnx-acp-test, and fallback path {}",
        fallback.display()
    );
    fallback
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
            if !contents.trim().is_empty() {
                return Ok(contents);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("cancel sentinel was not created"));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_call_tool_session_prompt_ctrl_c_cancels_session() {
    let sentinel_path = cancel_sentinel_path();
    let _ = std::fs::remove_file(&sentinel_path);

    let mut env = HashMap::new();
    env.insert(
        "ACP_CANCEL_SENTINEL".to_string(),
        sentinel_path.display().to_string(),
    );

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

    let client = manager
        .get_client("issue68")
        .expect("ACP test client should be initialized");
    let session_id = client.session_new().await.expect("create ACP test session");

    let session_id_for_call = session_id.clone();
    let (abort_tx, abort_rx) = tokio::sync::oneshot::channel::<()>();
    let call_task = tokio::spawn(async move {
        session_prompt_with_abort(
            &client,
            session_id_for_call,
            "please hang".to_string(),
            async move {
                let _ = abort_rx.await;
            },
        )
        .await
    });

    abort_tx.send(()).expect("trigger ACP abort");
    let result = call_task.await.expect("call task should join cleanly");
    result.expect_err("abort should fail ACP session prompt");

    let cancelled_session_id = wait_for_sentinel(&sentinel_path)
        .await
        .expect("mock server should record ACP cancellation");
    assert_eq!(cancelled_session_id.trim(), session_id);

    let _ = std::fs::remove_file(&sentinel_path);
}
