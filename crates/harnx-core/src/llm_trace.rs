//! Optional file-based trace of every LLM API call and its response.
//!
//! When the `HARNX_LLM_TRACE` env var is set to a path, harnx writes one
//! JSON line per event to that file:
//!
//! - `kind: "request"` — the URL and body of an outgoing chat-completions
//!   call, captured at the lowest layer where the request body is fully
//!   built (after patches and header interpolation).
//! - `kind: "response"` — the parsed JSON body of a non-streaming response.
//! - `kind: "stream-event"` — one entry per parsed streaming event (one SSE
//!   message, or one Bedrock event-stream frame).
//!
//! This is independent of the standard `simplelog` log filter so the trace
//! file contains only LLM I/O and nothing else, even with debug logging
//! disabled. When the env var is unset, every entry point is a cheap
//! `OnceLock::get().is_none()` check, so call sites can invoke unconditionally.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

const ENV_VAR: &str = "HARNX_LLM_TRACE";

static TRACE_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// Open the trace file if `HARNX_LLM_TRACE` is set. The first successful
/// init wins; subsequent calls are no-ops. Open failures are reported on
/// stderr — we never fail the process for trace failures.
pub fn init_from_env() {
    if TRACE_FILE.get().is_some() {
        return;
    }
    let Ok(path) = std::env::var(ENV_VAR) else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => {
            let _ = TRACE_FILE.set(Mutex::new(file));
        }
        Err(err) => {
            eprintln!(
                "harnx: failed to open LLM trace file {}: {err}",
                path.display()
            );
        }
    }
}

pub fn is_enabled() -> bool {
    TRACE_FILE.get().is_some()
}

fn timestamp() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn write_entry(value: Value) {
    let Some(lock) = TRACE_FILE.get() else {
        return;
    };
    let line = match serde_json::to_string(&value) {
        Ok(s) => s,
        Err(_) => return,
    };
    if let Ok(mut file) = lock.lock() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

/// Trace an outgoing LLM HTTP request with a JSON body.
pub fn request(url: &str, body: &Value) {
    if !is_enabled() {
        return;
    }
    write_entry(json!({
        "ts": timestamp(),
        "kind": "request",
        "url": url,
        "body": body,
    }));
}

/// Trace an outgoing LLM HTTP request whose body is a raw byte string
/// (e.g. an AWS SigV4-signed Bedrock body that's already serialized by the
/// time we have access to it).
pub fn request_raw(url: &str, body: &str) {
    if !is_enabled() {
        return;
    }
    match serde_json::from_str::<Value>(body) {
        Ok(parsed) => write_entry(json!({
            "ts": timestamp(),
            "kind": "request",
            "url": url,
            "body": parsed,
        })),
        Err(_) => write_entry(json!({
            "ts": timestamp(),
            "kind": "request",
            "url": url,
            "body_raw": body,
        })),
    }
}

/// Trace a non-streaming LLM HTTP response body (parsed JSON).
pub fn response(provider: &str, body: &Value) {
    if !is_enabled() {
        return;
    }
    write_entry(json!({
        "ts": timestamp(),
        "kind": "response",
        "provider": provider,
        "body": body,
    }));
}

/// Trace one streaming event (one SSE message or one event-stream frame).
pub fn stream_event(provider: &str, body: &Value) {
    if !is_enabled() {
        return;
    }
    write_entry(json!({
        "ts": timestamp(),
        "kind": "stream-event",
        "provider": provider,
        "body": body,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::sync::Mutex;

    /// `init_from_env` writes to a process-wide `OnceLock`, so direct tests
    /// of the public path conflict across the suite. Exercise the same
    /// write path against a private file handle so the helpers'
    /// serialization is still covered.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn read_lines(path: &std::path::Path) -> Vec<Value> {
        let f = std::fs::File::open(path).unwrap();
        BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(&l).expect("trace line is valid JSON"))
            .collect()
    }

    fn write_via_handle(file: &Mutex<File>, value: Value) {
        let line = serde_json::to_string(&value).unwrap();
        let mut f = file.lock().unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[test]
    fn write_entry_serializes_request_response_and_stream_event() {
        let _g = TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("harnx-llm-trace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        let _ = std::fs::remove_file(&path);
        let file = Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap(),
        );

        write_via_handle(
            &file,
            json!({
                "ts": "test",
                "kind": "request",
                "url": "https://api.example.com/v1/messages",
                "body": {"model": "claude-3-7-sonnet", "messages": []},
            }),
        );
        write_via_handle(
            &file,
            json!({
                "ts": "test",
                "kind": "response",
                "provider": "claude",
                "body": {"id": "msg_abc"},
            }),
        );
        write_via_handle(
            &file,
            json!({
                "ts": "test",
                "kind": "stream-event",
                "provider": "claude",
                "body": {"type": "message_start"},
            }),
        );

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["kind"], "request");
        assert_eq!(lines[0]["url"], "https://api.example.com/v1/messages");
        assert_eq!(lines[1]["kind"], "response");
        assert_eq!(lines[1]["provider"], "claude");
        assert_eq!(lines[2]["kind"], "stream-event");
        assert_eq!(lines[2]["body"]["type"], "message_start");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
