#![cfg(test)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Script describing mock responses for each turn.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MockOpenAiScript {
    #[serde(default)]
    pub turns: Vec<MockOpenAiTurn>,
    #[serde(default = "default_fallback_text")]
    pub fallback_text: String,
    #[serde(default)]
    pub chunk_delay_ms: u64,
}

fn default_fallback_text() -> String {
    "No more scripted responses.".to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MockOpenAiTurn {
    #[serde(default)]
    pub text_chunks: Vec<String>,
    #[serde(default)]
    pub tool_calls: Vec<MockOpenAiToolCall>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MockOpenAiToolCall {
    pub name: String,
    pub arguments: Value,
    #[serde(default)]
    pub id: Option<String>,
}

struct ServerState {
    script: MockOpenAiScript,
    turn_index: AtomicUsize,
    request_log: Mutex<Vec<Value>>,
}

impl ServerState {
    fn new(script: MockOpenAiScript) -> Self {
        Self {
            script,
            turn_index: AtomicUsize::new(0),
            request_log: Mutex::new(Vec::new()),
        }
    }

    fn next_turn(&self) -> MockOpenAiTurn {
        let idx = self.turn_index.fetch_add(1, Ordering::SeqCst);
        self.script
            .turns
            .get(idx)
            .cloned()
            .unwrap_or_else(|| MockOpenAiTurn {
                text_chunks: vec![self.script.fallback_text.clone()],
                tool_calls: vec![],
            })
    }

    fn log_request(&self, request: Value) {
        if let Ok(mut log) = self.request_log.lock() {
            log.push(request);
        }
    }
}

pub struct MockOpenAiServer {
    port: u16,
    shutdown: Option<TcpStream>,
    accept_thread: Option<JoinHandle<()>>,
}

impl MockOpenAiServer {
    pub fn start(script: MockOpenAiScript) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind mock server")?;
        listener
            .set_nonblocking(false)
            .context("failed to configure mock server listener")?;
        let port = listener
            .local_addr()
            .context("failed to get mock server address")?
            .port();

        let shutdown_listener =
            TcpListener::bind("127.0.0.1:0").context("failed to bind shutdown listener")?;
        let shutdown_addr = shutdown_listener
            .local_addr()
            .context("failed to get shutdown listener address")?;
        let shutdown =
            TcpStream::connect(shutdown_addr).context("failed to connect shutdown channel")?;
        let (shutdown_server, _) = shutdown_listener
            .accept()
            .context("failed to accept shutdown channel")?;

        let state = Arc::new(ServerState::new(script));
        let accept_thread =
            thread::spawn(move || run_accept_loop(listener, shutdown_server, state));

        Ok(Self {
            port,
            shutdown: Some(shutdown),
            accept_thread: Some(accept_thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for MockOpenAiServer {
    fn drop(&mut self) {
        if let Some(mut shutdown) = self.shutdown.take() {
            let _ = shutdown.write_all(b"shutdown");
            let _ = shutdown.flush();
            let _ = shutdown.shutdown(Shutdown::Both);
        }
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

fn run_accept_loop(listener: TcpListener, mut shutdown: TcpStream, state: Arc<ServerState>) {
    let _ = listener.set_nonblocking(true);
    let _ = shutdown.set_nonblocking(true);

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }

        let mut buf = [0; 16];
        match shutdown.read(&mut buf) {
            Ok(0) => {}
            Ok(_) => break,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn handle_connection(mut stream: TcpStream, state: Arc<ServerState>) {
    // Read the request in a loop until we have the complete headers
    let mut buffer = Vec::new();
    let mut temp_buf = [0u8; 1024];
    let headers_end_pos = loop {
        match stream.read(&mut temp_buf) {
            Ok(0) => return, // EOF
            Ok(n) => {
                buffer.extend_from_slice(&temp_buf[..n]);
                // Check if we have the full headers (ending with \r\n\r\n)
                if let Some(pos) = buffer
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                {
                    break pos + 4;
                }
                // Prevent unbounded growth while waiting for headers
                if buffer.len() > 1_048_576 {
                    // 1MB limit for headers
                    return;
                }
            }
            Err(_) => return,
        }
    };

    // Parse headers to extract Content-Length
    let headers_str = match String::from_utf8(buffer[..headers_end_pos].to_vec()) {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut content_length = 0usize;
    for line in headers_str.lines() {
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().unwrap_or(0);
            break;
        }
    }

    // Read the body based on Content-Length
    let body_bytes_read = buffer.len() - headers_end_pos;
    if body_bytes_read < content_length {
        let remaining = content_length - body_bytes_read;
        let current_len = buffer.len();
        buffer.resize(current_len + remaining, 0);
        let mut total_read = 0;
        while total_read < remaining {
            match stream.read(&mut buffer[current_len + total_read..]) {
                Ok(0) => return, // EOF before complete body
                Ok(n) => total_read += n,
                Err(_) => return,
            }
        }
    }

    // Now parse the complete request
    let request_str = String::from_utf8_lossy(&buffer);
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
    let body = if content_length > 0 && headers_end_pos < buffer.len() {
        String::from_utf8_lossy(&buffer[headers_end_pos..headers_end_pos + content_length])
    } else {
        "".into()
    };

    match (method, path) {
        ("GET", "/v1/models") => write_json_response(&mut stream, &handle_list_models()),
        ("POST", "/v1/chat/completions") => {
            let body_json: Value =
                serde_json::from_str(&body).unwrap_or_else(|_| json!({"error": "Invalid JSON"}));
            state.log_request(body_json.clone());
            handle_chat_completions(body_json, &state, &mut stream);
        }
        _ => write_http_response(
            &mut stream,
            "404 Not Found",
            "application/json",
            &json!({"error": "Not found"}).to_string(),
        ),
    }
}

fn handle_list_models() -> Value {
    json!({
        "object": "list",
        "data": [{
            "id": "mock-llm",
            "object": "model",
            "created": 0,
            "owned_by": "mock"
        }]
    })
}

fn handle_chat_completions(request: Value, state: &ServerState, stream: &mut TcpStream) {
    let is_stream = request
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let turn = state.next_turn();
    let full_text = turn.text_chunks.join("");
    let tool_calls: Vec<Value> = turn
        .tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| {
            json!({
                "id": tc.id.clone().unwrap_or_else(|| format!("call_{i}")),
                "type": "function",
                "function": {
                    "name": tc.name,
                    "arguments": tc.arguments.to_string()
                }
            })
        })
        .collect();

    if is_stream {
        write_streaming_response(stream, &full_text, &tool_calls);
    } else {
        write_json_response(
            stream,
            &build_non_streaming_response(&full_text, tool_calls),
        );
    }
}

fn write_streaming_response(stream: &mut TcpStream, text: &str, tool_calls: &[Value]) {
    let mut body = String::new();

    if !text.is_empty() {
        body.push_str(&format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-llm",
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": text},
                    "finish_reason": Value::Null
                }]
            })
        ));
    }

    if !tool_calls.is_empty() {
        body.push_str(&format!(
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
        ));
    } else {
        body.push_str(&format!(
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
        ));
    }

    body.push_str("data: [DONE]\n\n");
    write_http_response(stream, "200 OK", "text/event-stream", &body);
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
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": if has_tool_calls { "tool_calls" } else { "stop" }
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0
        }
    })
}

fn write_json_response(stream: &mut TcpStream, response: &Value) {
    write_http_response(stream, "200 OK", "application/json", &response.to_string());
}

fn write_http_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let http_response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(http_response.as_bytes());
    let _ = stream.flush();
}