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

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[cfg(unix)]
use std::os::unix::net::UnixListener;

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

/// A single turn in mock conversation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MockTurn {
    /// Text chunks to stream back.
    #[serde(default)]
    pub text_chunks: Vec<String>,

    /// Tool calls to include in response.
    #[serde(default)]
    pub tool_calls: Vec<MockToolCallDef>,
}

/// A tool call definition in script.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MockToolCallDef {
    pub name: String,
    pub arguments: Value,

    /// Optional: ID for tool call (auto-generated if not provided).
    #[serde(default)]
    pub id: Option<String>,
}

/// Global state for mock server.
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

#[derive(Debug, Clone, Default)]
struct ServerArgs {
    port: u16,
    host: Option<String>,
    script_path: Option<PathBuf>,
}

fn main() {
    let args = parse_args();
    let script = load_script(args.script_path.as_deref());
    let state = Arc::new(ServerState::new(script));

    if let Some(host) = args.host.as_deref() {
        if host.ends_with(".sock") {
            #[cfg(unix)]
            {
                run_unix_server(host, state);
                return;
            }
            #[cfg(not(unix))]
            {
                eprintln!("Unix socket mode is only supported on Unix platforms");
                std::process::exit(1);
            }
        }
    }

    run_tcp_server(args.port, state);
}

fn parse_args() -> ServerArgs {
    let mut args = std::env::args().skip(1);
    let mut parsed = ServerArgs {
        port: 3829,
        ..Default::default()
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" | "-p" => {
                if let Some(value) = args.next() {
                    if let Ok(port) = value.parse::<u16>() {
                        parsed.port = port;
                    }
                }
            }
            "--host" => {
                parsed.host = args.next();
            }
            "--script" | "-s" => {
                if let Some(value) = args.next() {
                    parsed.script_path = Some(PathBuf::from(value));
                }
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => {
                if takes_value(&arg) {
                    let _ = args.next();
                }
            }
        }
    }

    parsed
}

fn takes_value(flag: &str) -> bool {
    // Only the known llama-server value-flags consume the following argument.
    // Other unknown flags are treated as bare booleans and ignored, so a flag
    // like `--verbose` does not accidentally swallow the next argument.
    matches!(flag, "-m" | "-hf" | "-c" | "-ngl" | "-t")
}

fn print_help() {
    println!("harnx-mock-llm - Standalone mock LLM server for TUI repro workflows");
    println!();
    println!("Usage: harnx-mock-llm [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --port, -p <PORT>      Port to listen on (default: 3829)");
    println!("  --host <HOST|PATH>     Host/socket to listen on (.sock => Unix socket mode)");
    println!("  --script, -s <FILE>    YAML script file defining responses");
    println!("  --help, -h             Show this help message");
    println!();
    println!("Unknown flags are ignored so this can mimic llama-server CLI.");
}

fn load_script(script_path: Option<&Path>) -> MockScript {
    if let Some(path) = script_path {
        match std::fs::read_to_string(path) {
            Ok(content) => match serde_yaml::from_str::<MockScript>(&content) {
                Ok(script) => script,
                Err(err) => {
                    eprintln!("Failed to parse script file: {err}");
                    std::process::exit(1);
                }
            },
            Err(err) => {
                eprintln!("Failed to read script file: {err}");
                std::process::exit(1);
            }
        }
    } else {
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
    }
}

fn run_tcp_server(port: u16, state: Arc<ServerState>) {
    let addr = format!("127.0.0.1:{port}");
    let listener = match std::net::TcpListener::bind(&addr) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("Failed to bind to {addr}: {err}");
            std::process::exit(1);
        }
    };

    println!("READY listening on {addr}");
    io::stdout().flush().ok();

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(err) => eprintln!("Connection failed: {err}"),
        }
    }
}

#[cfg(unix)]
fn run_unix_server(socket_path: &str, state: Arc<ServerState>) {
    let socket_path = PathBuf::from(socket_path);
    if let Some(parent) = socket_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!(
                "Failed to create socket parent dir {}: {err}",
                parent.display()
            );
            std::process::exit(1);
        }
    }

    if socket_path.exists() {
        match std::fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(err) => {
                eprintln!(
                    "Failed to remove stale socket {}: {err}",
                    socket_path.display()
                );
                std::process::exit(1);
            }
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("Failed to bind to {}: {err}", socket_path.display());
            std::process::exit(1);
        }
    };

    println!("READY listening on {}", socket_path.display());
    io::stdout().flush().ok();

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(err) => eprintln!("Connection failed: {err}"),
        }
    }
}

fn handle_connection<S>(mut stream: S, state: Arc<ServerState>)
where
    S: Read + Write,
{
    let mut buffer = [0; 65536];
    let bytes_read = match stream.read(&mut buffer) {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request_str = String::from_utf8_lossy(&buffer[..bytes_read]);
    let mut lines = request_str.lines();
    let request_line = match lines.next() {
        Some(line) => line,
        None => return,
    };

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }

    let method = parts[0];
    let path = parts[1];
    let body_start = request_str.find("\r\n\r\n").map(|i| i + 4);
    let body = body_start.map(|start| &request_str[start..]).unwrap_or("");

    match (method, path) {
        ("GET", "/health") => handle_health(&mut stream),
        ("POST", "/v1/chat/completions") => handle_chat_completions(&mut stream, body, state),
        _ => write_not_found(&mut stream),
    }
}

fn handle_health(stream: &mut impl Write) {
    let body = b"{\"status\":\"ok\"}";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn handle_chat_completions(stream: &mut impl Write, body: &str, state: Arc<ServerState>) {
    let request_json: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
    state.log_request(request_json.clone());

    let is_streaming = request_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (response_text, tool_calls) = if let Some(turn) = state.current_turn() {
        let text = turn.text_chunks.join("");
        let tool_calls = turn
            .tool_calls
            .iter()
            .enumerate()
            .map(|(i, tc)| {
                json!({
                    "id": tc.id.clone().unwrap_or_else(|| format!("call_{}", i + 1)),
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments.to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        state.advance_turn();
        (text, tool_calls)
    } else {
        (state.script.fallback_text.clone(), vec![])
    };

    if is_streaming {
        write_streaming_response(
            stream,
            &response_text,
            tool_calls,
            state.script.chunk_delay_ms,
        );
    } else {
        let response = build_non_streaming_response(&response_text, tool_calls);
        let body = response.to_string();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    }
}

fn write_not_found(stream: &mut impl Write) {
    let body = "Not Found";
    let response = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn write_streaming_response(
    stream: &mut impl Write,
    text: &str,
    tool_calls: Vec<Value>,
    chunk_delay_ms: u64,
) {
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.flush();

    let chunks = chunk_text_for_stream(text);

    for chunk in chunks {
        let event = format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-llm",
                "choices": [{
                    "index": 0,
                    "delta": {"content": chunk},
                    "finish_reason": Value::Null
                }]
            })
        );
        let _ = stream.write_all(event.as_bytes());
        let _ = stream.flush();
        if chunk_delay_ms > 0 {
            thread::sleep(Duration::from_millis(chunk_delay_ms));
        }
    }

    if !tool_calls.is_empty() {
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

/// Chunks text for SSE streaming, preserving all whitespace including newlines.
///
/// Each chunk ends at a word boundary, including trailing whitespace.
/// The concatenation of all chunks exactly equals the input text.
fn chunk_text_for_stream(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }

    let mut chunks = Vec::new();
    let mut current_chunk = String::new();

    for ch in text.chars() {
        current_chunk.push(ch);
        // End chunk after each word (non-whitespace followed by whitespace)
        // This produces multiple small chunks that preserve all characters.
        if ch.is_whitespace() && !current_chunk.trim().is_empty() {
            chunks.push(std::mem::take(&mut current_chunk));
        }
    }

    // Push any remaining content
    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_text_preserves_newlines() {
        let input = "line1\n  line2\n\n```rust\nfn x(){}\n```\n";
        let chunks = chunk_text_for_stream(input);
        let reconstructed = chunks.join("");
        assert_eq!(
            input, reconstructed,
            "Chunks must preserve exact text including newlines"
        );
    }

    #[test]
    fn test_chunk_empty_input() {
        let chunks = chunk_text_for_stream("");
        assert!(chunks.is_empty(), "Empty input should yield empty chunks");
    }

    #[test]
    fn test_chunk_single_word() {
        let input = "hello";
        let chunks = chunk_text_for_stream(input);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn test_chunk_word_with_space() {
        let input = "hello ";
        let chunks = chunk_text_for_stream(input);
        assert_eq!(chunks, vec!["hello "]);
    }

    #[test]
    fn test_chunk_multiple_words() {
        let input = "hello world";
        let chunks = chunk_text_for_stream(input);
        assert_eq!(chunks, vec!["hello ", "world"]);
        assert_eq!(chunks.join(""), input);
    }

    #[test]
    fn test_chunk_preserves_indentation() {
        let input = "  indented\n    more indented";
        let chunks = chunk_text_for_stream(input);
        assert_eq!(chunks.join(""), input);
    }

    #[test]
    fn test_chunk_preserves_blank_lines() {
        let input = "line1\n\nline3";
        let chunks = chunk_text_for_stream(input);
        assert_eq!(chunks.join(""), input);
    }

    #[test]
    fn test_chunk_preserves_trailing_newline() {
        let input = "line\n";
        let chunks = chunk_text_for_stream(input);
        assert_eq!(chunks.join(""), input);
    }

    #[test]
    fn test_chunk_code_fence() {
        let input = "```rust\ncode\n```\n";
        let chunks = chunk_text_for_stream(input);
        assert_eq!(chunks.join(""), input);
    }
}
