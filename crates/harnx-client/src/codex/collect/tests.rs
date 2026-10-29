use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct EventCount(std::sync::atomic::AtomicUsize);

#[tokio::test]
async fn non_streaming_codex_retains_partial_output_and_usage_at_the_output_limit() {
    let builder = response(&[json!({"type": "response.incomplete", "response": {
        "id": "resp_partial", "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "partial answer"}]}],
        "usage": {"input_tokens": 10, "output_tokens": 20}
    }})]).await;
    let output = completion(builder, &Model::new("codex", "gpt-6-luna"))
        .await
        .unwrap();
    assert_eq!(
        (output.text.as_str(), output.output_tokens),
        ("partial answer", Some(20))
    );
}

impl harnx_core::event::AgentEventSink for EventCount {
    fn emit(&self, _: harnx_core::event::AgentEvent) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn response(events: &[Value]) -> reqwest::RequestBuilder {
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            assert_ne!(socket.read(&mut byte).await.unwrap(), 0);
            header.extend_from_slice(&byte);
        }
        // Codex can omit Content-Type. A single connection catches replays.
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
    });
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
        .get(format!("http://{address}/responses"))
}

#[tokio::test]
async fn non_streaming_codex_preserves_completed_text_reasoning_calls_and_usage() {
    let builder = response(&[
        json!({"type": "response.output_text.delta", "delta": "Read."}),
        json!({"type": "response.completed", "response": {
            "id": "resp_1", "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "Read."}]},
                {"type": "reasoning", "encrypted_content": "opaque", "summary": [{"type": "summary_text", "text": "Reason."}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{}"}
            ], "usage": {"input_tokens": 10, "output_tokens": 4, "input_tokens_details": {"cached_tokens": 2}}
        }})
    ]).await;
    let sink = std::sync::Arc::new(EventCount::default());
    let model = Model::new("codex", "gpt-6-luna");
    let output = harnx_core::sink::with_agent_event_sink(sink.clone(), completion(builder, &model))
        .await
        .unwrap();
    assert_eq!(
        sink.0.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a buffered completion must not emit duplicate chunk events"
    );
    assert_eq!(
        (
            output.text.as_str(),
            output.thought.as_deref(),
            output.id.as_deref()
        ),
        ("Read.", Some("Reason."), Some("resp_1"))
    );
    assert_eq!(
        (
            output.input_tokens,
            output.output_tokens,
            output.cached_tokens
        ),
        (Some(10), Some(4), Some(2))
    );
    assert_eq!(
        (
            output.tool_calls[0].id.as_deref(),
            output.tool_calls[0].thought_signature.as_deref()
        ),
        (Some("call_1"), Some("opaque"))
    );
}

#[tokio::test]
async fn non_streaming_codex_requires_completion_and_keeps_typed_provider_errors() {
    let model = Model::new("codex", "gpt-6-luna");
    let builder =
        response(&[json!({"type": "response.output_text.delta", "delta": "partial"})]).await;
    assert!(completion(builder, &model)
        .await
        .unwrap_err()
        .to_string()
        .contains("before a terminal response"));
    let builder = response(&[json!({"type": "response.failed", "response": {"error": {"code": "context_length_exceeded", "message": "too large"}}})]).await;
    assert!(completion(builder, &model)
        .await
        .unwrap_err()
        .downcast_ref::<harnx_core::error::LlmError>()
        .unwrap()
        .is_context_overflow());
}
