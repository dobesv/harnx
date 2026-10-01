use super::*;
use crate::openai_responses::responses_streaming_with_content_type;
use harnx_core::{abort::create_abort_signal, error::LlmError, model::Model};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Raw HTTP keeps the absence of Content-Type exact; most mock response builders
// automatically add one when given a body. One connection also catches replays.
async fn response_builder(status: u16, headers: &str, body: &str) -> RequestBuilder {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let response = format!(
        "HTTP/1.1 {status} Test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        while !request.ends_with(b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0);
            request.extend_from_slice(&chunk[..read]);
        }
        // Exercise incremental decoding, including across UTF-8 boundaries.
        for chunk in response.as_bytes().chunks(7) {
            if socket.write_all(chunk).await.is_err() {
                break; // The client may finish at the terminal event or headers.
            }
            tokio::task::yield_now().await;
        }
    });
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .get(format!("http://{addr}/responses"))
}

fn response_events() -> String {
    let events = [
        json!({"type":"response.output_text.delta","delta":"OK 🦀"}),
        json!({"type":"response.output_item.added","item":{
            "type":"function_call","id":"fc_1","call_id":"call_1","name":"check"
        }}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{}"}),
        json!({"type":"response.function_call_arguments.done","item_id":"fc_1"}),
        json!({"type":"response.completed","response":{"usage":{
            "input_tokens":16,"output_tokens":24
        },"output":[]}}),
    ];
    events
        .iter()
        .map(|data| {
            format!(
                "event: {}\ndata: {data}\n\n",
                data["type"].as_str().unwrap()
            )
        })
        .collect()
}

async fn codex_response(status: u16, headers: &str, body: &str) -> Result<SseHandler> {
    let builder = response_builder(status, headers, body).await;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut handler = SseHandler::new(tx, create_abort_signal());
    responses_streaming_with_content_type(
        builder,
        &mut handler,
        &Model::new("pantheon/codex", "gpt-6-astra:max"),
        SseContentType::AllowMissing,
    )
    .await?;
    Ok(handler)
}

#[tokio::test]
async fn codex_stream_accepts_missing_empty_and_standard_content_type() {
    for headers in [
        "",
        "Content-Type: \r\n",
        "Content-Type: text/event-stream; charset=utf-8\r\n",
    ] {
        let handler = codex_response(200, headers, &response_events())
            .await
            .unwrap();
        let (text, _, calls, usage) = handler.take();
        assert_eq!(text, "OK 🦀");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(calls[0].name, "check");
        assert_eq!(calls[0].arguments, json!({}));
        assert_eq!((usage.input_tokens, usage.output_tokens), (16, 24));
    }
}

#[tokio::test]
async fn codex_stream_accepts_data_only_event_types() {
    let body = response_events()
        .lines()
        .filter(|line| !line.starts_with("event:"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let handler = codex_response(200, "", &body).await.unwrap();
    assert_eq!(handler.take().0, "OK 🦀");
}

#[tokio::test]
async fn codex_stream_requires_completion_even_when_content_type_is_missing() {
    for body in [
        "",
        "<html>SECRET_BODY</html>",
        "data: SECRET_BODY\n\n",
        "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
    ] {
        let error = codex_response(200, "", body).await.err().unwrap();
        assert!(error.to_string().contains("before response.completed"));
        assert!(!format!("{error:#}").contains("SECRET_BODY"));
    }
}

#[tokio::test]
async fn explicit_wrong_content_type_is_rejected_without_echoing_body() {
    for content_type in [SseContentType::AllowMissing, SseContentType::Required] {
        let builder = response_builder(200, "Content-Type: text/html\r\n", "SECRET_BODY").await;
        let error = sse_stream_with_content_type(builder, |_| Ok(false), content_type)
            .await
            .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("status: 200"));
        assert!(diagnostic.contains("text/html"));
        assert!(!diagnostic.contains("SECRET_BODY"));
    }
}

#[tokio::test]
async fn other_providers_still_require_content_type() {
    let builder = response_builder(200, "", &response_events()).await;
    let error = sse_stream(builder, |_| panic!("Must reject before parsing"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected text/event-stream"));
}

#[tokio::test]
async fn streaming_http_errors_preserve_status_and_retry_after_without_content_type() {
    for (status, body, expected) in [
        (
            429,
            r#"{"error":{"type":"rate_limit_error","message":"Wait briefly"}}"#,
            "Wait briefly",
        ),
        (
            401,
            r#"{"error":{"type":"authentication_error","message":"Log in again"}}"#,
            "Log in again",
        ),
        (
            400,
            r#"{"detail":"Model not supported"}"#,
            "Model not supported",
        ),
        (503, "<html>SECRET_BODY</html>", "non-JSON error response"),
    ] {
        let error = codex_response(status, "Retry-After: 2\r\n", body)
            .await
            .err()
            .unwrap();
        let provider_error = error.downcast_ref::<LlmError>().unwrap();
        assert_eq!(provider_error.status, status);
        assert_eq!(provider_error.retry_after, Some(Duration::from_secs(2)));
        assert!(provider_error.message.contains(expected));
        assert!(!format!("{error:#}").contains("SECRET_BODY"));
    }
}

#[tokio::test]
async fn codex_stream_propagates_in_band_errors_without_content_type() {
    let body = "event: response.failed\ndata: {\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"Try again\"}}}\n\n";
    let error = codex_response(200, "", body).await.err().unwrap();
    assert!(format!("{error:#}").contains("Try again"));
}

#[tokio::test]
async fn missing_content_type_stream_honors_handler_stop() {
    let builder = response_builder(200, "", "data: first\n\ndata: second\n\n").await;
    let mut messages = Vec::new();
    sse_stream_with_content_type(
        builder,
        |message| {
            messages.push(message.data);
            Ok(true)
        },
        SseContentType::AllowMissing,
    )
    .await
    .unwrap();
    assert_eq!(messages, ["first"]);
}

#[tokio::test]
async fn codex_stream_stops_at_completion() {
    let body = response_events() + "event: error\ndata: {\"message\":\"Must not be read\"}\n\n";
    let handler = codex_response(200, "", &body).await.unwrap();
    assert_eq!(handler.take().0, "OK 🦀");
}

#[tokio::test]
async fn aborted_codex_stream_does_not_require_completion() {
    let builder = response_builder(200, "", "data: {}\n\n").await;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let abort = create_abort_signal();
    abort.set_ctrlc();
    let mut handler = SseHandler::new(tx, abort);
    responses_streaming_with_content_type(
        builder,
        &mut handler,
        &Model::new("codex", "gpt-6-astra:max"),
        SseContentType::AllowMissing,
    )
    .await
    .unwrap();
    assert!(handler.take().0.is_empty());
}

fn live_handler() -> (SseHandler, tokio::sync::mpsc::UnboundedReceiver<SseEvent>) {
    // The receiver must stay alive: a failed send ends the stream too.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (SseHandler::new(tx, create_abort_signal()), rx)
}

/// Send `unit` until about `chars` characters have gone out, stopping at the
/// first chunk the handler rejects.
fn stream_repeatedly(
    mut send: impl FnMut(&str) -> Result<()>,
    unit: &str,
    chars: usize,
) -> Result<()> {
    for _ in 0..chars.div_ceil(unit.chars().count()) {
        send(unit)?;
    }
    Ok(())
}

/// Stream `unit` into one channel of a fresh handler until the guard stops
/// it, and name the channel the guard blamed.
fn stopped_channel(
    send: impl Fn(&mut SseHandler, &str) -> Result<()>,
    unit: &str,
) -> harnx_core::loop_guard::OutputChannel {
    let (mut handler, _rx) = live_handler();
    let err = stream_repeatedly(|chunk| send(&mut handler, chunk), unit, 4_000)
        .expect_err("the repeat stops the stream");
    let repeat = harnx_core::loop_guard::find_repetitive_output(&err).expect("a repeat");
    repeat.channel
}

#[test]
fn a_repeating_answer_ends_the_stream() {
    let channel = stopped_channel(SseHandler::text, "the ");
    assert_eq!(channel, harnx_core::loop_guard::OutputChannel::Answer);
}

#[test]
fn repeating_thinking_is_reported_as_thinking() {
    let channel = stopped_channel(SseHandler::thought, "Let me check again. ");
    assert_eq!(channel, harnx_core::loop_guard::OutputChannel::Thinking);
}

#[test]
fn answer_and_thinking_are_counted_separately() {
    // 1,200 characters of "the " on each channel: 2,400 together, but neither
    // channel alone reaches the threshold.
    let (mut handler, _rx) = live_handler();
    for _ in 0..300 {
        handler.thought("the ").unwrap();
        handler.text("the ").unwrap();
    }
}

#[test]
fn a_disabled_guard_stops_nothing() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut handler = SseHandler::new(tx, create_abort_signal()).with_output_guard(false);
    stream_repeatedly(|text| handler.text(text), "the ", 4_000).unwrap();
    stream_repeatedly(|text| handler.thought(text), "the ", 4_000).unwrap();
}
