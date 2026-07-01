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
    fn spawn(token_cmd: &str) -> Option<Self> {
        let script_path = script_path();
        let python = match find_python3() {
            Some(python) => python,
            None => {
                eprintln!("skipping gcp hook script test: python3 not present");
                return None;
            }
        };

        let mut child = Command::new(python)
            .arg(script_path)
            .env("HARNX_GCP_TOKEN_CMD", token_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn gcp auth hook script");

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
        .join("../../example_config/gcp-auth-hook.py")
        .canonicalize()
        .expect("canonicalize gcp auth hook path")
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
    let dir = std::env::temp_dir().join(format!("harnx-gcp-hook-{name}-{millis}-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn resident_gcp_hook_handles_metadata_and_auth_injection() {
    let mut process = match HookProcess::spawn("printf 'test-token\n'") {
        Some(process) => process,
        None => return,
    };

    let metadata = process.request(json!({
        "id": "evt-1",
        "method": "GET",
        "host": "metadata.google.internal",
        "path": "/computeMetadata/v1/instance/service-accounts/default/token",
        "headers": {"metadata-flavor": "google"}
    }));
    assert_eq!(metadata["id"], "evt-1");
    assert_eq!(metadata["respond"]["status"], 200);
    assert_eq!(metadata["respond"]["headers"]["metadata-flavor"], "Google");
    assert_eq!(
        metadata["respond"]["headers"]["content-type"],
        "application/json"
    );
    let metadata_body: Value = serde_json::from_str(
        metadata["respond"]["body"]
            .as_str()
            .expect("metadata body string"),
    )
    .expect("metadata body json");
    assert!(!metadata_body["access_token"]
        .as_str()
        .expect("metadata token string")
        .is_empty());
    assert_eq!(metadata_body["token_type"], "Bearer");
    assert!(
        metadata_body["expires_in"]
            .as_i64()
            .expect("expires_in integer")
            > 0
    );

    let bigquery = process.request(json!({
        "id": "evt-2",
        "method": "GET",
        "host": "bigquery.googleapis.com",
        "path": "/bigquery/v2/projects/demo/jobs",
        "headers": {}
    }));
    assert_eq!(bigquery["id"], "evt-2");
    assert_eq!(bigquery["headers"]["authorization"], "Bearer test-token");
    assert!(bigquery.get("respond").is_none());

    let passthrough = process.request(json!({
        "id": "evt-3",
        "method": "GET",
        "host": "example.com",
        "path": "/hello",
        "headers": {"x-demo": "1"}
    }));
    assert_eq!(passthrough, json!({"id": "evt-3"}));
}

#[test]
fn resident_gcp_hook_mints_once_and_reuses_cached_token() {
    let dir = unique_temp_dir("token-cache");
    let counter_path = dir.join("counter.txt");
    let token_cmd = format!(
        "count_file={} ; count=0; if [ -f \"$count_file\" ]; then count=$(cat \"$count_file\"); fi; count=$((count+1)); printf '%s' \"$count\" > \"$count_file\"; printf 'cached-token\\n'",
        shell_single_quote(&counter_path.display().to_string())
    );

    let mut process = match HookProcess::spawn(&token_cmd) {
        Some(process) => process,
        None => return,
    };

    for id in ["evt-11", "evt-12", "evt-13"] {
        let response = process.request(json!({
            "id": id,
            "method": "GET",
            "host": "bigquery.googleapis.com",
            "path": "/bigquery/v2/projects/demo/jobs",
            "headers": {}
        }));
        assert_eq!(response["headers"]["authorization"], "Bearer cached-token");
    }

    let count = std::fs::read_to_string(counter_path).expect("read counter file");
    assert_eq!(count.trim(), "1");
}

fn shell_single_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}
