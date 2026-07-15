#![cfg(unix)]

use harnx_proxy_auth::exec_hook::ExecHookProcess;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use tempfile::TempDir;

#[cfg(unix)]
static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
fn test_dir(name: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_millis();
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("harnx-exec-hook-{name}-{millis}-{id}"));
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

#[cfg(unix)]
fn python_echo_script(state_path: &Path, ready: bool) -> String {
    std::thread::sleep(std::time::Duration::from_millis(50));
    format!(
        r#"#!/usr/bin/env python3
import json
import pathlib
import sys

state = pathlib.Path({state_path})
count = 0
if state.exists():
    count = int(state.read_text())
count += 1
state.write_text(str(count))
if {ready}:
    print("READY", flush=True)

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    req = json.loads(raw)
    req.setdefault("headers", {{}})
    req["headers"]["x-processed-by"] = req["id"]
    print(json.dumps(req), flush=True)
"#,
        state_path =
            serde_json::to_string(&state_path.display().to_string()).expect("state path json"),
        ready = if ready { "True" } else { "False" }
    )
}

#[cfg(unix)]
fn python_concurrent_script() -> String {
    std::thread::sleep(std::time::Duration::from_millis(50));
    r#"#!/usr/bin/env python3
import json
import sys
import threading
import time

def reply(req):
    time.sleep((int(req["path"].split("-")[-1]) % 3) * 0.05)
    req.setdefault("headers", {})
    req["headers"]["x-id"] = req["id"]
    req["headers"]["x-path"] = req["path"]
    print(json.dumps(req), flush=True)

print("READY", flush=True)
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    req = json.loads(raw)
    threading.Thread(target=reply, args=(req,), daemon=True).start()
"#
    .to_string()
}

#[cfg(unix)]
fn python_timeout_script() -> String {
    std::thread::sleep(std::time::Duration::from_millis(50));
    r#"#!/usr/bin/env python3
import json
import sys
import threading
import time

def reply(req):
    if req["path"] == "/slow":
        time.sleep(2.0)
    req.setdefault("headers", {})
    req["headers"]["x-after"] = req["path"]
    print(json.dumps(req), flush=True)

print("READY", flush=True)
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    req = json.loads(raw)
    threading.Thread(target=reply, args=(req,), daemon=True).start()
"#
    .to_string()
}

#[cfg(unix)]
fn python_exit_after_first_request_script() -> String {
    std::thread::sleep(std::time::Duration::from_millis(50));
    r#"#!/usr/bin/env python3
import json
import sys

print("READY", flush=True)
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    req = json.loads(raw)
    if req["path"] == "/first":
        sys.exit(1)
    print(json.dumps(req), flush=True)
"#
    .to_string()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawns_once_and_reuses_process() {
    let dir = test_dir("spawn-once");
    let state_path = dir.join("count.txt");
    let process =
        ExecHookProcess::spawn_inline(&python_echo_script(&state_path, false), 1).unwrap();

    let first = process
        .transform(json!({"method":"GET","host":"example.com","path":"/one","headers":{}}))
        .await;
    let second = process
        .transform(json!({"method":"GET","host":"example.com","path":"/two","headers":{}}))
        .await;

    assert_eq!(first["path"], "/one");
    assert_eq!(second["path"], "/two");
    assert!(first["headers"]["x-processed-by"]
        .as_str()
        .unwrap()
        .starts_with("evt-"));
    assert!(second["headers"]["x-processed-by"]
        .as_str()
        .unwrap()
        .starts_with("evt-"));
    assert_eq!(fs::read_to_string(state_path).unwrap().trim(), "1");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_carries_safe_vars_to_hook() {
    let script = r#"#!/usr/bin/env python3
import json
import sys
print("READY", flush=True)
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    req = json.loads(raw)
    req.setdefault("headers", {})
    req["headers"]["x-temp-root"] = req.get("vars", {}).get("temp_file_root", "MISSING")
    print(json.dumps(req), flush=True)
"#;
    let process = ExecHookProcess::spawn_inline(script, 1)
        .unwrap()
        .with_request_vars(std::sync::Arc::new(json!({
            "temp_file_root": "/tmp/harnx-fs-test",
            "fake_hex_key": "deadbeef",
        })));

    let out = process
        .transform(json!({"method":"GET","host":"example.com","path":"/","headers":{}}))
        .await;

    assert_eq!(out["headers"]["x-temp-root"], "/tmp/harnx-fs-test");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn correlates_concurrent_requests_by_id() {
    let process =
        std::sync::Arc::new(ExecHookProcess::spawn_inline(&python_concurrent_script(), 1).unwrap());
    let mut tasks = Vec::new();

    for idx in 0..8 {
        let process = process.clone();
        tasks.push(tokio::spawn(async move {
            let path = format!("/item-{idx}");
            let result = process
                .transform(json!({
                    "method": "GET",
                    "host": "example.com",
                    "path": path,
                    "headers": {}
                }))
                .await;
            (idx, result)
        }));
    }

    for task in tasks {
        let (idx, result) = task.await.unwrap();
        let expected_path = format!("/item-{idx}");
        assert_eq!(result["path"], Value::String(expected_path.clone()));
        assert_eq!(result["headers"]["x-path"], Value::String(expected_path));
        assert!(result["headers"]["x-id"]
            .as_str()
            .unwrap()
            .starts_with("evt-"));
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_returns_original_and_next_request_succeeds() {
    let process = ExecHookProcess::spawn_inline(&python_timeout_script(), 1).unwrap();

    let original = json!({"method":"GET","host":"example.com","path":"/slow","headers":{"a":"b"}});
    let timed_out = process.transform(original.clone()).await;
    let fast = process
        .transform(json!({"method":"GET","host":"example.com","path":"/fast","headers":{}}))
        .await;

    assert_eq!(timed_out, original);
    assert_eq!(fast["headers"]["x-after"], "/fast");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_triggers_lazy_respawn() {
    let process =
        ExecHookProcess::spawn_inline(&python_exit_after_first_request_script(), 1).unwrap();

    let original = json!({"method":"GET","host":"example.com","path":"/first","headers":{}});
    let first = process.transform(original.clone()).await;
    let second = process
        .transform(json!({"method":"GET","host":"example.com","path":"/second","headers":{}}))
        .await;

    assert_eq!(first, original);
    assert_eq!(second["path"], "/second");
    assert_eq!(second["headers"], json!({}));
}

#[cfg(not(unix))]
#[test]
fn spawn_errors_on_non_unix() {
    assert!(ExecHookProcess::spawn_inline("#!/bin/sh\nexit 0\n", 1).is_err());
}

#[cfg(unix)]
fn write_executable_hook(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executable_path_hook_runs_directly() {
    let temp = TempDir::new().unwrap();
    let hook_path = temp.path().join("resident-hook.py");
    write_executable_hook(
        &hook_path,
        r#"#!/usr/bin/env python3
import json
import sys

print("READY", flush=True)
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    headers = msg.setdefault("headers", {})
    headers["x-hook-source"] = "path"
    print(json.dumps(msg), flush=True)
"#,
    );

    let process = ExecHookProcess::spawn_path(hook_path, 1).unwrap();
    let req = json!({"method":"GET","host":"example.com","path":"/p","headers":{"accept":"application/json"}});
    let transformed = process.transform(req).await;

    assert_eq!(transformed["headers"]["x-hook-source"], "path");
    assert_eq!(transformed["headers"]["accept"], "application/json");
}

#[cfg(unix)]
#[test]
fn non_executable_path_hook_fails_up_front() {
    let temp = TempDir::new().unwrap();
    let hook_path = temp.path().join("resident-hook.py");
    std::fs::write(
        &hook_path,
        "#!/bin/sh
exit 0
",
    )
    .unwrap();

    let err = ExecHookProcess::spawn_path(hook_path.clone(), 1)
        .err()
        .expect("non-executable path should fail");
    assert_eq!(
        err.to_string(),
        format!("--hook path {} is not executable", hook_path.display())
    );
}

#[cfg(unix)]
#[test]
fn missing_path_hook_fails_up_front() {
    let temp = TempDir::new().unwrap();
    let hook_path = temp.path().join("does-not-exist.py");

    let err = ExecHookProcess::spawn_path(hook_path.clone(), 1)
        .err()
        .expect("missing path should fail");
    assert_eq!(
        err.to_string(),
        format!("--hook path {} not found", hook_path.display())
    );
}

#[cfg(unix)]
#[test]
fn directory_path_hook_fails_up_front() {
    // A directory is executable (has the exec bit) but is not a file.
    let temp = TempDir::new().unwrap();
    let dir_path = temp.path().join("a-directory");
    std::fs::create_dir(&dir_path).unwrap();

    let err = ExecHookProcess::spawn_path(dir_path.clone(), 1)
        .err()
        .expect("directory path should fail");
    assert_eq!(
        err.to_string(),
        format!("--hook path {} is not a file", dir_path.display())
    );
}
