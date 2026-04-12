//! harnx-mock-llm: A standalone mock LLM server for local TUI repro workflows.
//!
//! This binary provides an HTTP server that mimics an OpenAI-compatible API
//! with deterministic, scriptable responses. It's designed for local debugging
//! and TUI reproduction workflows outside of `cargo test`.
//!
//! # Usage
//!
//! ```bash
//! # Start server with default responses
//! harnx-mock-llm --port 3829
//!
//! # Start with a script file
//! harnx-mock-llm --port 3829 --script /path/to/script.yaml
//! ```
//!
//! # Script Format
//!
//! The script file is YAML with the following structure:
//!
//! ```yaml
//! turns:
//!   - text_chunks:
//!       - "Hello"
//!       - " world"
//!     tool_calls:
//!       - name: "Bash"
//!         arguments: { "command": "echo test" }
//!   - text_chunks:
//!       - "Second response"
//! ```
//!
//! Each turn is consumed sequentially as requests arrive.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Script describing mock responses for each turn.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MockScript {
    /// List of turns. Each turn is consumed by one chat completion request.
    #[serde(default)]
    pub turns: Vec<MockTurn>,

    /// Default response when no more turns are available.
    #[serde(default = "default_fallback_text")]
    pub fallback_text: String,

    /// Delay in milliseconds between chunks.
    #[serde(default)]
    pub chunk_delay_ms: u64,
}

fn default_fallback_text() -> String {
    "No more scripted responses.".to_string()
}

/// A single turn in the mock conversation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MockTurn {
    /// Text chunks to stream back.
    #[serde(default)]
    pub text_chunks: Vec<String>,

    /// Tool calls to include in the response.
    #[serde(default)]
    pub tool_calls: Vec<MockToolCallDef>,
}

/// A tool call definition in the script.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MockToolCallDef {
    pub name: String,
    pub arguments: Value,

    /// Optional: ID for the tool call (auto-generated if not provided).
    #[serde(default)]
    pub id: Option<String>,
}

/// Global state for the mock server.
struct ServerState {
    script: MockScript,
    turn_index: AtomicUsize,
    request_log: Mutex<Vec<Value>>,
}

impl ServerState {
    fn new(script: MockScript) -> Self {
        Self {
            script,
            turn_index: AtomicUsize::new(0),
            request_log: Mutex::new(Vec::new()),
        }
    }

    fn current_turn(&self) -> Option<MockTurn> {
        let idx = self.turn_index.load(Ordering::SeqCst);
        self.script.turns.get(idx).cloned()
    }

    fn advance_turn(&self) {
        self.turn_index.fetch_add(1, Ordering::SeqCst);
    }

    fn log_request(&self, request: Value) {
        if let Ok(mut log) = self.request_log.lock() {
            log.push(request);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut port: u16 = 3829;
    let mut script_path: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" | "-p" => {
                if let Some(value) = args.next() {
                    if let Ok(p) = value.parse::<u16>() {
                        port = p;
                    }
                }
            }
            "--script" | "-s" => {
                if let Some(value) = args.next() {
                    script_path = Some(PathBuf::from(value));
                }
            }
            "--help" | "-h" => {
                println!("harnx-mock-llm - Standalone mock LLM server for TUI repro workflows");
                println!();
                println!("Usage: harnx-mock-llm [OPTIONS]");
                println!();
                println!("Options:");
                println!("  --port, -p <PORT>      Port to listen on (default: 3829)");
                println!("  --script, -s <FILE>    YAML script file defining responses");
                println!("  --help, -h             Show this help message");
                println!();
                println!("The server provides an OpenAI-compatible API at http://localhost:<PORT>");
                println!();
                println!("Script format (YAML):");
                println!("  turns:");
                println!("    - text_chunks: [\"Hello\", \" world\"]");
                println!("      tool_calls:");
                println!("        - name: Bash");
                println!("          arguments: {{command: \"echo test\"}}");
                return;
            }
            _ => {
                eprintln!("Unknown argument: {}", arg);
                eprintln!("Use --help for usage information");
                std::process::exit(1);
            }
        }
    }

    let script = if let Some(path) = script_path {
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_yaml::from_str::<MockScript>(&content) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to parse script file: {}", e);
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("Failed to read script file: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        // Default script with a simple response
        MockScript {
            turns: vec![
                MockTurn {
                    text_chunks: vec!["Hello! ".to_string(), "I'm a mock LLM.".to_string()],
                    tool_calls: vec![],
                },
                MockTurn {
                    text_chunks: vec!["Second response.".to_string()],
                    tool_calls: vec![],
                },
            ],
            fallback_text: "No more scripted responses.".to_string(),
            chunk_delay_ms: 50,
        }
    };

    let state = Arc::new(ServerState::new(script));

    // Run a simple HTTP server using std::net
    let addr = format!("127.0.0.1:{}", port);
    let listener = match std::net::TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind to {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    // Signal ready
    println!("READY listening on {}", addr);
    io::stdout().flush().ok();

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    handle_connection(stream, state);
                });
            }
            Err(e) => {
                eprintln!("Connection failed: {}", e);
            }
        }
    }
}

fn handle_connection(mut stream: std::net::TcpStream, state: Arc<ServerState>) {
    use std::io::Read;

    let mut buffer = [0; 65536];
    let bytes_read = match stream.read(&mut buffer) {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request_str = String::from_utf8_lossy(&buffer[..bytes_read]);

    // Parse HTTP request
    let mut lines = request_str.lines();
    let request_line = match lines.next() {
        Some(l) => l,
        None => return,
    };

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }

    let method = parts[0];
    let path = parts[1];

    // Find body (after double newline)
    let body_start = request_str.find("\r\n\r\n").map(|i| i + 4);
    let body = body_start.map(|start| &request_str[start..]).unwrap_or("");

    // Route the request
    match (method, path) {
        ("GET", "/v1/models") => {
            let response = handle_list_models();
            let response_str = response.to_string();
            let http_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                response_str.len(),
                response_str
            );
            let _ = stream.write_all(http_response.as_bytes());
            let _ = stream.flush();
        }
        ("POST", "/v1/chat/completions") => {
            let body_json: Value = match serde_json::from_str(body) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Failed to parse request body: {}", e);
                    json!({"error": "Invalid JSON"})
                }
            };
            state.log_request(body_json.clone());
            handle_chat_completions(body_json, &state, &mut stream);
        }
        _ => {
            let response = json!({"error": "Not found"});
            let response_str = response.to_string();
            let http_response = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                response_str.len(),
                response_str
            );
            let _ = stream.write_all(http_response.as_bytes());
            let _ = stream.flush();
        }
    }
}

fn handle_list_models() -> Value {
    json!({
        "object": "list",
        "data": [
            {
                "id": "mock-llm",
                "object": "model",
                "created": 0,
                "owned_by": "mock"
            }
        ]
    })
}

fn handle_chat_completions(request: Value, state: &ServerState, stream: &mut std::net::TcpStream) {
    let is_stream = request
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let turn = state.current_turn();
    state.advance_turn();

    let turn = match turn {
        Some(t) => t,
        None => MockTurn {
            text_chunks: vec![state.script.fallback_text.clone()],
            tool_calls: vec![],
        },
    };

    // Build tool calls array
    let tool_calls: Vec<Value> = turn
        .tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| {
            json!({
                "id": tc.id.clone().unwrap_or_else(|| format!("call_{}", i)),
                "type": "function",
                "function": {
                    "name": tc.name,
                    "arguments": tc.arguments.to_string()
                }
            })
        })
        .collect();

    if is_stream {
        write_streaming_response(
            stream,
            &turn.text_chunks,
            state.script.chunk_delay_ms,
            &tool_calls,
        );
    } else {
        let full_text = turn.text_chunks.join("");
        let response = build_non_streaming_response(&full_text, tool_calls);
        let response_str = response.to_string();
        let http_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_str.len(),
            response_str
        );
        let _ = stream.write_all(http_response.as_bytes());
        let _ = stream.flush();
    }
}

fn write_streaming_response(
    stream: &mut std::net::TcpStream,
    text_chunks: &[String],
    chunk_delay_ms: u64,
    tool_calls: &[Value],
) {
    // Write HTTP headers for SSE (no Content-Length so we can flush per-event).
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.flush();

    let delay = Duration::from_millis(chunk_delay_ms);

    // Emit one SSE event per text chunk, preserving ordering.
    for (i, chunk) in text_chunks.iter().enumerate() {
        if chunk.is_empty() {
            continue;
        }
        if i > 0 && chunk_delay_ms > 0 {
            thread::sleep(delay);
        }
        let event = format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-llm",
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": chunk},
                    "finish_reason": serde_json::Value::Null
                }]
            })
        );
        let _ = stream.write_all(event.as_bytes());
        let _ = stream.flush();
    }

    // Emit tool_calls or stop event.
    if !tool_calls.is_empty() {
        if chunk_delay_ms > 0 && !text_chunks.is_empty() {
            thread::sleep(delay);
        }
        let event = format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-llm",
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": tool_calls},
                    "finish_reason": "tool_calls"
                }]
            })
        );
        let _ = stream.write_all(event.as_bytes());
        let _ = stream.flush();
    } else {
        let event = format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-llm",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            })
        );
        let _ = stream.write_all(event.as_bytes());
        let _ = stream.flush();
    }

    let _ = stream.write_all(b"data: [DONE]\n\n");
    let _ = stream.flush();
}

fn build_non_streaming_response(text: &str, tool_calls: Vec<Value>) -> Value {
    let has_tool_calls = !tool_calls.is_empty();

    let message = if has_tool_calls {
        json!({
            "role": "assistant",
            "content": text,
            "tool_calls": tool_calls
        })
    } else {
        json!({
            "role": "assistant",
            "content": text
        })
    };

    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-llm",
        "choices": [
            {
                "index": 0,
                "message": message,
                "finish_reason": if has_tool_calls { "tool_calls" } else { "stop" }
            }
        ],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        }
    })
}
