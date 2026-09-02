use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::Response;
use serde_json::Value;

pub(crate) type AppResponse =
    Response<http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>>;

#[derive(Debug, Default)]
pub(crate) struct SseRead {
    pub(crate) frames: Vec<String>,
    pub(crate) events: Vec<Value>,
    pub(crate) comments: Vec<String>,
}

async fn read_sse_body<F>(
    response: AppResponse,
    inner_timeout: Duration,
    done: &F,
) -> anyhow::Result<SseRead>
where
    F: Fn(&SseRead) -> bool,
{
    let mut body = response.into_body().into_data_stream();
    let mut read = SseRead::default();
    let mut partial = String::new();

    while !done(&read) {
        match tokio::time::timeout(inner_timeout, futures_util::StreamExt::next(&mut body)).await {
            Ok(Some(Ok(chunk))) => {
                partial.push_str(std::str::from_utf8(&chunk).expect("sse utf8"));
            }
            Ok(Some(Err(err))) => panic!(
                "error while reading SSE stream before predicate satisfied: {err}. frames: {:?}, events: {:?}, comments: {:?}",
                read.frames, read.events, read.comments
            ),
            Ok(None) => break,
            Err(_) => anyhow::bail!(
                "timed out after {inner_timeout:?} waiting for SSE predicate. frames: {:?}, events: {:?}, comments: {:?}",
                read.frames,
                read.events,
                read.comments
            ),
        }
        if parse_sse_frames(&mut partial, &mut read, done) {
            break;
        }
    }
    Ok(read)
}

fn parse_sse_frames<F>(partial: &mut String, read: &mut SseRead, done: &F) -> bool
where
    F: Fn(&SseRead) -> bool,
{
    while let Some(idx) = partial.find("\n\n") {
        let frame = partial[..idx].trim().to_string();
        partial.drain(..idx + 2);
        if frame.is_empty() {
            continue;
        }
        read.frames.push(frame.clone());
        if frame.starts_with(':') {
            read.comments.push(frame);
        } else {
            let payload = frame
                .strip_prefix("data: ")
                .expect("sse frame should start with data prefix");
            read.events
                .push(serde_json::from_str(payload).expect("valid event json"));
        }
        if done(read) {
            return true;
        }
    }
    false
}

pub(crate) async fn read_sse_until<F>(response: AppResponse, timeout: Duration, done: F) -> SseRead
where
    F: Fn(&SseRead) -> bool,
{
    tokio::time::timeout(timeout, read_sse_body(response, timeout, &done))
        .await
        .expect("SSE read should finish before outer timeout")
        .unwrap_or_else(|error| panic!("{error:#}"))
}
