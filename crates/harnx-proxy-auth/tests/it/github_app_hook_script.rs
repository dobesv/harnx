#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(1);

struct HookProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl HookProcess {
    fn spawn(envs: &[(&str, &str)]) -> Option<Self> {
        let script_path = script_path();
        let python = match find_python3() {
            Some(python) => python,
            None => {
                eprintln!("skipping github app hook script test: python3 not present");
                return None;
            }
        };

        let mut command = Command::new(python);
        command
            .arg(script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in envs {
            command.env(key, value);
        }

        let mut child = command.spawn().expect("spawn github app auth hook script");
        let stdin = child.stdin.take().expect("hook stdin");
        let stdout = child.stdout.take().expect("hook stdout");
        let mut process = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };

        let ready = process.read_line_with_timeout(Duration::from_secs(2));
        assert_eq!(ready.as_deref(), Some("READY"));
        Some(process)
    }

    fn request(&mut self, request: Value) -> Value {
        writeln!(
            self.stdin,
            "{}",
            serde_json::to_string(&request).expect("serialize request")
        )
        .expect("write request");
        self.stdin.flush().expect("flush request");

        let line = self
            .read_line_with_timeout(Duration::from_secs(2))
            .expect("hook response line");
        serde_json::from_str(&line).expect("parse hook response")
    }

    fn read_line_with_timeout(&mut self, timeout: Duration) -> Option<String> {
        let reader = &mut self.stdout;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut line = String::new();
                let result = reader.read_line(&mut line);
                let _ = tx.send((result, line));
            });

            match rx.recv_timeout(timeout) {
                Ok((Ok(0), _)) => None,
                Ok((Ok(_), line)) => Some(line.trim_end_matches(['\r', '\n']).to_owned()),
                Ok((Err(err), _)) => panic!("read hook output: {err}"),
                Err(_) => panic!("timed out waiting for hook output"),
            }
        })
    }
}

impl Drop for HookProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../example_config/github-app-auth-hook.py")
        .canonicalize()
        .expect("canonicalize github app auth hook path")
}

fn find_python3() -> Option<&'static str> {
    for candidate in ["python3", "/usr/bin/env"] {
        let mut command = Command::new(candidate);
        if candidate == "/usr/bin/env" {
            command.arg("python3");
        }
        if command
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .is_some_and(|status| status.success())
        {
            return Some(candidate);
        }
    }
    None
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before unix epoch")
        .as_millis();
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("harnx-github-app-hook-{name}-{millis}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn resident_github_app_hook_injects_api_and_git_headers_and_passthrough() {
    let temp_dir = unique_temp_dir("override");
    let counter_path = temp_dir.join("counter.txt");
    let counter_string = counter_path.display().to_string();
    let expires_at = "2999-01-01T00:00:00Z";
    let envs = [
        (
            "GITHUB_APP_INSTALLATION_TOKEN",
            "ghs_test_installation_token",
        ),
        ("GITHUB_APP_INSTALLATION_TOKEN_EXPIRES_AT", expires_at),
        ("GITHUB_APP_TEST_COUNTER_FILE", counter_string.as_str()),
    ];
    let mut process = match HookProcess::spawn(&envs) {
        Some(process) => process,
        None => return,
    };

    let api = process.request(json!({
        "id": "evt-1",
        "method": "GET",
        "host": "api.github.com",
        "path": "/repos/octo/demo/issues",
        "headers": {}
    }));
    assert_eq!(api["id"], "evt-1");
    assert_eq!(
        api["headers"]["authorization"],
        "Bearer ghs_test_installation_token"
    );

    let git = process.request(json!({
        "id": "evt-2",
        "method": "GET",
        "host": "github.com",
        "path": "/octo/demo.git/info/refs",
        "headers": {}
    }));
    assert_eq!(git["id"], "evt-2");
    assert_eq!(
        git["headers"]["authorization"],
        "Basic eC1hY2Nlc3MtdG9rZW46Z2hzX3Rlc3RfaW5zdGFsbGF0aW9uX3Rva2Vu"
    );

    let passthrough = process.request(json!({
        "id": "evt-3",
        "method": "GET",
        "host": "example.com",
        "path": "/hello",
        "headers": {"x-demo": "1"}
    }));
    assert_eq!(passthrough, json!({"id": "evt-3"}));

    let count = std::fs::read_to_string(counter_path).expect("read counter file");
    assert_eq!(count.trim(), "1");
}

#[test]
fn resident_github_app_hook_caches_installation_token_across_requests() {
    let temp_dir = unique_temp_dir("cache");
    let counter_path = temp_dir.join("counter.txt");
    let counter_string = counter_path.display().to_string();
    let expires_at = "2999-01-01T00:00:00Z";
    let envs = [
        (
            "GITHUB_APP_INSTALLATION_TOKEN",
            "ghs_cached_installation_token",
        ),
        ("GITHUB_APP_INSTALLATION_TOKEN_EXPIRES_AT", expires_at),
        ("GITHUB_APP_TEST_COUNTER_FILE", counter_string.as_str()),
    ];
    let mut process = match HookProcess::spawn(&envs) {
        Some(process) => process,
        None => return,
    };

    for id in ["evt-11", "evt-12", "evt-13"] {
        let response = process.request(json!({
            "id": id,
            "method": "GET",
            "host": "uploads.github.com",
            "path": "/repos/octo/demo/releases/assets/1",
            "headers": {}
        }));
        assert_eq!(
            response["headers"]["authorization"],
            "Bearer ghs_cached_installation_token"
        );
    }

    let count = std::fs::read_to_string(counter_path).expect("read counter file");
    assert_eq!(count.trim(), "1");
}
