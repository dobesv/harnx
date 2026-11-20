#![cfg(test)]
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

use super::*;
use crate::cli::HookDef;

const KILL_RETRY_COUNT: usize = 50;
const KILL_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const SCRIPT_PERMISSIONS_MODE: u32 = 0o755;

fn write_mock_mcp_script(path: &Path, tool_env_json: &str, is_error: bool, server_name: &str) {
    let call_result = if is_error {
        r#""result": {"isError": true, "content": [{"type": "text", "text": "service error"}]}"#
            .to_string()
    } else {
        format!(r#""result": {{"content": [{{"type": "text", "text": {tool_env_json:?}}}]}}"#)
    };

    let script = format!(
        r#"#!/usr/bin/env python3
import sys, json, os

canary = sys.argv[1]
with open(canary, "w") as f:
    f.write(str(os.getpid()))

for line in sys.stdin:
    if not line.strip():
        continue
    req = json.loads(line)
    mid = req.get("id")
    method = req.get("method")
    if method == "initialize":
        resp = {{"jsonrpc": "2.0", "id": mid, "result": {{"protocolVersion": "2024-11-05", "capabilities": {{}}, "serverInfo": {{"name": "{server_name}", "version": "1.0"}}}}}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/call":
        resp = {{"jsonrpc": "2.0", "id": mid, {call_result}}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#
    );
    fs::write(path, script).expect("write mock script");
    fs::set_permissions(path, fs::Permissions::from_mode(SCRIPT_PERMISSIONS_MODE))
        .expect("chmod mock script");
}

fn assert_pid_alive(pid: i32) {
    let exit_status = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .expect("execute kill -0");
    assert!(exit_status.success(), "process {pid} should be alive");
}

fn assert_pid_dead(pid: i32) {
    for _ in 0..KILL_RETRY_COUNT {
        let status = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .expect("execute kill -0");
        if !status.success() {
            return;
        }
        std::thread::sleep(KILL_RETRY_INTERVAL);
    }
    panic!("process {pid} should be terminated");
}

fn read_canary_pid(canary: &Path) -> i32 {
    let pid_str = fs::read_to_string(canary)
        .unwrap_or_else(|e| panic!("failed to read canary file {}: {e}", canary.display()));
    pid_str
        .trim()
        .parse::<i32>()
        .unwrap_or_else(|e| panic!("invalid PID {pid_str:?} in {}: {e}", canary.display()))
}

fn assert_canary_pid_dead(canary: &Path) {
    let pid = read_canary_pid(canary);
    assert_pid_dead(pid);
}

#[tokio::test]
async fn test_run_hooks_precedence_classification_and_lifetime() {
    let temp = tempdir().expect("tempdir");
    let cred_script = temp.path().join("mock_cred.py");
    let proxy_script = temp.path().join("mock_proxy.py");
    let cred_canary = temp.path().join("cred_canary.txt");
    let proxy_canary = temp.path().join("proxy_canary.txt");

    write_mock_mcp_script(
        &cred_script,
        r#"{"env": {"AWS_REGION": "us-east-1", "SHARED": "from_cred"}, "filesystem": {}}"#,
        false,
        "mock-cred",
    );
    write_mock_mcp_script(
        &proxy_script,
        r#"{"env": {"HTTP_PROXY": "http://127.0.0.1:8080", "SHARED": "from_proxy"}, "filesystem": {}}"#,
        false,
        "harnx-proxy_auth-server",
    );

    let hooks = vec![
        HookDef {
            hook_type: "environment".to_string(),
            command: cred_script.to_string_lossy().into_owned(),
            args: vec![cred_canary.to_string_lossy().into_owned()],
        },
        HookDef {
            hook_type: "environment".to_string(),
            command: proxy_script.to_string_lossy().into_owned(),
            args: vec![proxy_canary.to_string_lossy().into_owned()],
        },
    ];

    let result = run_hooks(&hooks, temp.path()).await.expect("run_hooks");

    assert_eq!(
        result.credential_env.get("AWS_REGION").map(String::as_str),
        Some("us-east-1")
    );
    assert_eq!(
        result.credential_env.get("SHARED").map(String::as_str),
        Some("from_cred")
    );
    assert_eq!(
        result.proxy_env.get("HTTP_PROXY").map(String::as_str),
        Some("http://127.0.0.1:8080")
    );
    assert_eq!(
        result.proxy_env.get("SHARED").map(String::as_str),
        Some("from_proxy")
    );

    let cred_pid = read_canary_pid(&cred_canary);
    let proxy_pid = read_canary_pid(&proxy_canary);

    assert_pid_alive(cred_pid);
    assert_pid_alive(proxy_pid);

    result.manager.shutdown().await;

    assert_pid_dead(cred_pid);
    assert_pid_dead(proxy_pid);
}

async fn assert_hook_fails_closed(tool_env_json: &str, is_error: bool, server_name: &str) {
    let temp = tempdir().expect("tempdir");
    let script = temp.path().join("failing_mcp.py");
    let canary = temp.path().join("canary.txt");

    write_mock_mcp_script(&script, tool_env_json, is_error, server_name);

    let hooks = vec![HookDef {
        hook_type: "environment".to_string(),
        command: script.to_string_lossy().into_owned(),
        args: vec![canary.to_string_lossy().into_owned()],
    }];

    let res = run_hooks(&hooks, temp.path()).await;
    assert!(res.is_err(), "hook must fail closed");

    assert_canary_pid_dead(&canary);
}

#[tokio::test]
async fn test_run_hooks_fails_closed_on_is_error() {
    assert_hook_fails_closed("{}", true, "err-service").await;
}

#[tokio::test]
async fn test_run_hooks_fails_closed_on_malformed_result() {
    assert_hook_fails_closed(r#"{"env": {"BAD_PORT": 9999}}"#, false, "bad-service").await;
}

#[tokio::test]
async fn test_run_hooks_shuts_down_prior_services_on_subsequent_failure() {
    let temp = tempdir().expect("tempdir");
    let good_script = temp.path().join("good_mcp.py");
    let good_canary = temp.path().join("good_canary.txt");
    let bad_script = temp.path().join("bad_mcp.py");
    let bad_canary = temp.path().join("bad_canary.txt");

    write_mock_mcp_script(
        &good_script,
        r#"{"env": {"AWS_REGION": "us-east-1"}, "filesystem": {}}"#,
        false,
        "good-service",
    );
    write_mock_mcp_script(&bad_script, "{}", true, "bad-service");

    let hooks = vec![
        HookDef {
            hook_type: "environment".to_string(),
            command: good_script.to_string_lossy().into_owned(),
            args: vec![good_canary.to_string_lossy().into_owned()],
        },
        HookDef {
            hook_type: "environment".to_string(),
            command: bad_script.to_string_lossy().into_owned(),
            args: vec![bad_canary.to_string_lossy().into_owned()],
        },
    ];

    let res = run_hooks(&hooks, temp.path()).await;
    assert!(res.is_err(), "run_hooks must fail if any hook fails");

    let good_pid = read_canary_pid(&good_canary);
    assert_pid_dead(good_pid);

    assert_canary_pid_dead(&bad_canary);
}

#[tokio::test]
async fn test_run_hooks_fails_closed_on_handshake_failure() {
    let temp = tempdir().expect("tempdir");
    let script = temp.path().join("crash_mcp.py");
    let canary = temp.path().join("canary.txt");

    let py = r#"#!/usr/bin/env python3
import sys, json, os, time

canary = sys.argv[1]
with open(canary, "w") as f:
    f.write(str(os.getpid()))
    f.flush()

for line in sys.stdin:
    if not line.strip():
        continue
    # Send malformed non-JSON response during initialize
    sys.stdout.write("MALFORMED_HANDSHAKE\n")
    sys.stdout.flush()
    # Sleep to confirm the supervisor kills and reaps it on handshake error
    time.sleep(30)
"#;
    fs::write(&script, py).expect("write crash script");
    fs::set_permissions(&script, fs::Permissions::from_mode(SCRIPT_PERMISSIONS_MODE))
        .expect("chmod crash script");

    let hooks = vec![HookDef {
        hook_type: "environment".to_string(),
        command: script.to_string_lossy().into_owned(),
        args: vec![canary.to_string_lossy().into_owned()],
    }];

    let res = run_hooks(&hooks, temp.path()).await;
    assert!(res.is_err(), "run_hooks must fail on handshake failure");

    assert_canary_pid_dead(&canary);
}
