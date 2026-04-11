//! harnx-acp-test: Mock ACP server for testing cancellation behavior.
//!
//! This binary implements a minimal ACP server that supports session/prompt
//! with hanging behavior to test SIGINT/cancellation handling.
//!
//! Environment variables:
//!   ACP_CANCEL_SENTINEL - Path to write the session ID when cancellation is received

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

static RUNNING: AtomicBool = AtomicBool::new(true);

fn cancel_sentinel_path() -> Option<PathBuf> {
    std::env::var("ACP_CANCEL_SENTINEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

fn record_cancel(session_id: &str) {
    if let Some(path) = cancel_sentinel_path() {
        if let Err(e) = std::fs::write(&path, session_id) {
            eprintln!("harnx-acp-test: failed to write cancel sentinel: {e}");
        }
    }
}

fn send_response(msg_id: u64, result: Value) {
    let message = json!({
        "jsonrpc": "2.0",
        "id": msg_id,
        "result": result
    });
    println!("{message}");
}

fn send_error(msg_id: u64, code: i64, message: &str) {
    let message = json!({
        "jsonrpc": "2.0",
        "id": msg_id,
        "error": {
            "code": code,
            "message": message
        }
    });
    println!("{message}");
}

struct SessionState {
    current_session_id: std::sync::Mutex<Option<String>>,
    next_session: AtomicU64,
    cancel_flag: AtomicBool,
}

impl SessionState {
    fn new() -> Self {
        Self {
            current_session_id: std::sync::Mutex::new(None),
            next_session: AtomicU64::new(1),
            cancel_flag: AtomicBool::new(false),
        }
    }

    fn create_session(&self) -> String {
        let id = self.next_session.fetch_add(1, Ordering::SeqCst);
        let session_id = format!("session-{id}");
        *self.current_session_id.lock().unwrap() = Some(session_id.clone());
        self.cancel_flag.store(false, Ordering::SeqCst);
        session_id
    }

    fn current_session(&self) -> Option<String> {
        self.current_session_id.lock().unwrap().clone()
    }

    fn is_cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
    }

    fn set_cancelled(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }
}

fn prompt_worker(msg_id: u64, _session_id: String, state: Arc<SessionState>) {
    // Simulate a long-running prompt that checks for cancellation
    while RUNNING.load(Ordering::SeqCst) && !state.is_cancelled() {
        thread::sleep(Duration::from_millis(50));
    }

    if state.is_cancelled() {
        // Don't send response if cancelled
        return;
    }

    send_response(
        msg_id,
        json!({
            "content": [{"type": "text", "text": "completed"}]
        }),
    );
}

fn handle_request(message: Value, state: Arc<SessionState>) {
    let msg_id = message.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
    let method = message.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(json!({}));

    eprintln!("METHOD:{method}");

    match method {
        "initialize" => {
            send_response(
                msg_id,
                json!({
                    "protocolVersion": 1,
                    "capabilities": {},
                    "agentCapabilities": {}
                }),
            );
        }
        "session/new" => {
            let session_id = state.create_session();
            send_response(msg_id, json!({"sessionId": session_id}));
        }
        "session/prompt" => {
            let session_id = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| state.current_session())
                .unwrap_or_else(|| "unknown".to_string());

            let state_clone = Arc::clone(&state);
            thread::spawn(move || {
                prompt_worker(msg_id, session_id, state_clone);
            });
        }
        "notifications/cancel" => {
            let session_id = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| state.current_session())
                .unwrap_or_else(|| "unknown".to_string());

            state.set_cancelled();
            record_cancel(&session_id);
            println!("CANCELLED:{session_id}");
            io::stdout().flush().ok();
        }
        "session/cancel" => {
            let session_id = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| state.current_session())
                .unwrap_or_else(|| "unknown".to_string());

            state.set_cancelled();
            record_cancel(&session_id);
            send_response(msg_id, json!({}));
        }
        "session/load" => {
            send_response(msg_id, json!({}));
        }
        _ => {
            send_error(msg_id, -32601, &format!("Unknown method: {method}"));
        }
    }
}

fn main() {
    // Ignore SIGINT so we can test cancellation properly
    #[cfg(unix)]
    {
        // Set SIGINT handler to ignore - this lets the process survive SIGINT
        // so the test can verify that the ACP cancellation notification is sent
        // and received before the process terminates.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
    }

    // Signal ready
    println!("READY");
    io::stdout().flush().ok();

    let state = Arc::new(SessionState::new());
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    while RUNNING.load(Ordering::SeqCst) {
        let line = match lines.next() {
            Some(Ok(l)) => l,
            Some(Err(_)) | None => break,
        };

        let trimmed = line.trim();
        if trimmed == "STOP" {
            break;
        }

        let message: Value = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(_) => continue,
        };

        handle_request(message, Arc::clone(&state));
    }
}
