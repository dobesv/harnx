use crate::{catch_error, parse_retry_after, CompletionTokenUsage, ToolCall};
use harnx_core::abort::AbortSignal;

use anyhow::{anyhow, bail, Context, Result};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use reqwest::RequestBuilder;
use reqwest_eventsource::{Error as EventSourceError, Event, RequestBuilderExt};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, Default)]
pub struct StreamingUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

pub struct SseHandler {
    sender: UnboundedSender<SseEvent>,
    abort_signal: AbortSignal,
    buffer: String,
    thought_buffer: String,
    thought_closed: bool,
    tool_calls: Vec<ToolCall>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

impl SseHandler {
    pub fn new(sender: UnboundedSender<SseEvent>, abort_signal: AbortSignal) -> Self {
        Self {
            sender,
            abort_signal,
            buffer: String::new(),
            thought_buffer: String::new(),
            thought_closed: false,
            tool_calls: Vec::new(),
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
        }
    }

    pub fn text(&mut self, text: &str) -> Result<()> {
        // debug!("HandleText: {}", text);
        if text.is_empty() {
            return Ok(());
        }
        if self.abort_signal.aborted() {
            return Ok(());
        }
        let prefix = if !self.thought_buffer.is_empty() && !self.thought_closed {
            self.thought_closed = true;
            "\n</think>\n\n"
        } else {
            ""
        };
        self.buffer.push_str(text);
        let ret = self
            .sender
            .send(SseEvent::Text(format!("{prefix}{text}")))
            .with_context(|| "Failed to send SseEvent:Text");
        if let Err(err) = ret {
            if self.abort_signal.aborted() {
                return Ok(());
            }
            return Err(err);
        }

        // Per-chunk AgentEvent emission. Best-effort; no-op if no sink
        // is installed. Emits the raw chunk text WITHOUT the display
        // prefix (<think> close / etc.) — that's channel plumbing only.
        {
            use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
            harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
                blocks: vec![ContentBlock::Text(text.to_string())],
            }));
        }

        Ok(())
    }

    pub fn thought(&mut self, thought: &str) -> Result<()> {
        if thought.is_empty() {
            return Ok(());
        }
        if self.abort_signal.aborted() {
            return Ok(());
        }
        let prefix = if self.thought_buffer.is_empty() {
            "<think>\n"
        } else {
            ""
        };
        self.thought_buffer.push_str(thought);
        let ret = self
            .sender
            .send(SseEvent::Text(format!("{prefix}{thought}")))
            .with_context(|| "Failed to send SseEvent:Text");
        if let Err(err) = ret {
            if self.abort_signal.aborted() {
                return Ok(());
            }
            return Err(err);
        }

        // Per-chunk AgentEvent emission. Best-effort; no-op if no sink
        // is installed. Emits raw thought text WITHOUT the <think>
        // bracketing — downstream sinks render their own conventions.
        {
            use harnx_core::event::{AgentEvent, ContentBlock, ModelEvent};
            harnx_core::sink::emit_agent_event(AgentEvent::Model(ModelEvent::ThoughtChunk {
                blocks: vec![ContentBlock::Text(thought.to_string())],
            }));
        }

        Ok(())
    }

    pub fn done(&mut self) {
        // debug!("HandleDone");
        if !self.thought_buffer.is_empty() && !self.thought_closed {
            self.thought_closed = true;
            let _ = self
                .sender
                .send(SseEvent::Text("\n</think>\n\n".to_string()));
        }
        let ret = self.sender.send(SseEvent::Done);
        if ret.is_err() {
            if self.abort_signal.aborted() {
                return;
            }
            warn!("Failed to send SseEvent:Done");
        }
    }

    pub fn tool_call(&mut self, call: ToolCall) -> Result<()> {
        // debug!("HandleCall: {:?}", call);
        if self.abort_signal.aborted() {
            return Ok(());
        }
        if !self.thought_buffer.is_empty() && !self.thought_closed {
            self.thought_closed = true;
            let _ = self
                .sender
                .send(SseEvent::Text("\n</think>\n\n".to_string()));
        }
        self.tool_calls.push(call);
        Ok(())
    }

    pub fn abort(&self) -> AbortSignal {
        self.abort_signal.clone()
    }

    pub fn aborted(&self) -> bool {
        self.abort_signal.aborted()
    }

    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }

    /// Attach thought_signature to tool calls that were emitted before
    /// reasoning.encrypted_content arrived. This supports the streaming case
    /// where function_call_arguments.done arrives before reasoning.output_item.done.
    pub fn attach_thought_signature_to_pending_tool_calls(
        &mut self,
        signature: String,
        provenance: harnx_core::tool::ReasoningProvenance,
    ) {
        self.tool_calls
            .iter_mut()
            .filter(|call| call.thought_signature.is_none())
            .for_each(|call| {
                call.thought_signature = Some(signature.clone());
                call.reasoning_provenance = Some(provenance.clone());
            });
    }

    pub fn set_usage(&mut self, usage: StreamingUsage) {
        if self.abort_signal.aborted() {
            return;
        }
        self.input_tokens = usage.input_tokens.or(self.input_tokens);
        self.output_tokens = usage.output_tokens.or(self.output_tokens);
        self.cached_tokens = usage.cached_tokens.or(self.cached_tokens);
        self.cache_write_tokens = usage.cache_write_tokens.or(self.cache_write_tokens);
    }

    pub fn take(self) -> (String, Option<String>, Vec<ToolCall>, CompletionTokenUsage) {
        let mut usage =
            CompletionTokenUsage::new(self.input_tokens, self.output_tokens, self.cached_tokens);
        usage.cache_write_tokens = self.cache_write_tokens.unwrap_or_default();
        let thought = if self.thought_buffer.is_empty() {
            None
        } else {
            Some(self.thought_buffer)
        };
        (self.buffer, thought, self.tool_calls, usage)
    }
}

#[derive(Debug)]
pub enum SseEvent {
    Text(String),
    Done,
}

#[derive(Debug)]
pub struct SseMmessage {
    #[allow(unused)]
    pub event: String,
    pub data: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseContentType {
    Required,
    /// Codex can omit Content-Type on successful SSE responses. Callers must
    /// validate the protocol's terminal event, since arbitrary bodies may parse
    /// as an empty SSE stream. Never relax an explicitly different media type.
    AllowMissing,
}

pub async fn sse_stream<F>(builder: RequestBuilder, handle: F) -> Result<()>
where
    F: FnMut(SseMmessage) -> Result<bool>,
{
    sse_stream_with_content_type(builder, handle, SseContentType::Required).await
}

pub(crate) async fn sse_stream_with_content_type<F>(
    builder: RequestBuilder,
    mut handle: F,
    content_type: SseContentType,
) -> Result<()>
where
    F: FnMut(SseMmessage) -> Result<bool>,
{
    let mut es = builder.eventsource()?;
    while let Some(event) = es.next().await {
        match event {
            Ok(Event::Open) => {}
            Ok(Event::Message(message)) => {
                let message = SseMmessage {
                    event: message.event,
                    data: message.data,
                };
                if handle(message)? {
                    break;
                }
            }
            Err(err) => {
                es.close();
                return handle_sse_error(err, content_type, handle).await;
            }
        }
    }
    Ok(())
}

async fn handle_sse_error<F>(
    err: EventSourceError,
    content_type: SseContentType,
    handle: F,
) -> Result<()>
where
    F: FnMut(SseMmessage) -> Result<bool>,
{
    match err {
        EventSourceError::StreamEnded => Ok(()),
        EventSourceError::InvalidStatusCode(_, res) => sse_status_error(res).await,
        EventSourceError::InvalidContentType(header, res) => {
            debug!(
                "Provider SSE response: status={}, content-type={:?}",
                res.status().as_u16(),
                header
            );
            if content_type == SseContentType::AllowMissing && header.as_bytes().is_empty() {
                // Consume this response, not a new request: retrying would discard
                // a paid completion and can duplicate tool calls or output.
                return sse_response_stream(res, handle).await;
            }
            // Do not read or print the body here: it may be an entire completion
            // containing prompts, tool output, and encrypted reasoning.
            bail!(
                "Invalid provider event-stream (status: {}, content-type: {:?}); expected text/event-stream",
                res.status().as_u16(),
                header
            )
        }
        EventSourceError::Parser(_) | EventSourceError::Utf8(_) => {
            bail!("Failed to parse provider event-stream")
        }
        _ => Err(err.into()),
    }
}

async fn sse_status_error(res: reqwest::Response) -> Result<()> {
    let status = res.status().as_u16();
    let retry_after = parse_retry_after(res.headers());
    debug!(
        "Provider SSE HTTP error: status={status}, content-type={:?}, retry-after={retry_after:?}",
        res.headers().get(reqwest::header::CONTENT_TYPE)
    );
    let data: Value = res.json().await.map_err(|_| harnx_core::error::LlmError {
        status,
        message: "Provider returned a non-JSON error response to a streaming request".into(),
        retry_after,
    })?;
    catch_error(&data, status, retry_after)?;
    bail!("Unexpected provider event-stream status: {status}; expected 200")
}

async fn sse_response_stream<F>(res: reqwest::Response, mut handle: F) -> Result<()>
where
    F: FnMut(SseMmessage) -> Result<bool>,
{
    let mut stream = res.bytes_stream().eventsource();
    while let Some(message) = stream.next().await {
        let message = message.map_err(|err| match err {
            eventsource_stream::EventStreamError::Transport(err) => {
                anyhow!(err).context("Failed to read provider event-stream")
            }
            _ => anyhow!("Failed to parse provider event-stream"),
        })?;
        if handle(SseMmessage {
            event: message.event,
            data: message.data,
        })? {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "stream_sse_tests.rs"]
mod sse_tests;

pub async fn json_stream<S, F, E>(mut stream: S, mut handle: F) -> Result<()>
where
    S: Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    F: FnMut(&str) -> Result<bool>,
    E: std::error::Error,
{
    let mut parser = JsonStreamParser::default();
    let mut unparsed_bytes = vec![];
    while let Some(chunk_bytes) = stream.next().await {
        let chunk_bytes =
            chunk_bytes.map_err(|err| anyhow!("Failed to read json stream, {err}"))?;
        unparsed_bytes.extend(chunk_bytes);
        match std::str::from_utf8(&unparsed_bytes) {
            Ok(text) => {
                if parser.process(text, &mut handle)? {
                    break;
                }
                unparsed_bytes.clear();
            }
            Err(_) => {
                continue;
            }
        }
    }
    if !unparsed_bytes.is_empty() {
        let text = std::str::from_utf8(&unparsed_bytes)?;
        parser.process(text, &mut handle)?;
    }

    Ok(())
}

#[derive(Debug, Default)]
struct JsonStreamParser {
    buffer: Vec<char>,
    cursor: usize,
    start: Option<usize>,
    balances: Vec<char>,
    quoting: bool,
    escape: bool,
}

impl JsonStreamParser {
    fn process<F>(&mut self, text: &str, handle: &mut F) -> Result<bool>
    where
        F: FnMut(&str) -> Result<bool>,
    {
        self.buffer.extend(text.chars());

        for i in self.cursor..self.buffer.len() {
            let ch = self.buffer[i];
            if self.quoting {
                if ch == '\\' {
                    self.escape = !self.escape;
                } else {
                    if !self.escape && ch == '"' {
                        self.quoting = false;
                    }
                    self.escape = false;
                }
                continue;
            }
            match ch {
                '"' => {
                    self.quoting = true;
                    self.escape = false;
                }
                '{' => {
                    if self.balances.is_empty() {
                        self.start = Some(i);
                    }
                    self.balances.push(ch);
                }
                '[' if self.start.is_some() => {
                    self.balances.push(ch);
                }
                '}' => {
                    self.balances.pop();
                    if self.balances.is_empty() {
                        if let Some(start) = self.start.take() {
                            let value: String = self.buffer[start..=i].iter().collect();
                            if handle(&value)? {
                                self.cursor = self.buffer.len();
                                return Ok(true);
                            }
                        }
                    }
                }
                ']' => {
                    self.balances.pop();
                }
                _ => {}
            }
        }
        self.cursor = self.buffer.len();
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use futures_util::stream;
    use rand::RngExt;

    fn split_chunks(text: &str) -> Vec<Vec<u8>> {
        let mut rng = rand::rng();
        let len = text.len();
        let cut1 = rng.random_range(1..len - 1);
        let cut2 = rng.random_range(cut1 + 1..len);
        let chunk1 = text.as_bytes()[..cut1].to_vec();
        let chunk2 = text.as_bytes()[cut1..cut2].to_vec();
        let chunk3 = text.as_bytes()[cut2..].to_vec();
        vec![chunk1, chunk2, chunk3]
    }

    macro_rules! assert_json_stream {
        ($input:expr, $output:expr) => {
            let chunks: Vec<_> = split_chunks($input)
                .into_iter()
                .map(|chunk| Ok::<_, std::convert::Infallible>(Bytes::from(chunk)))
                .collect();
            let stream = stream::iter(chunks);
            let mut output = vec![];
            let ret = json_stream(stream, |data| {
                output.push(data.to_string());
                Ok(false)
            })
            .await;
            assert!(ret.is_ok());
            assert_eq!($output.replace("\r\n", "\n"), output.join("\n"))
        };
    }

    #[tokio::test]
    async fn test_json_stream_ndjson() {
        let data = r#"{"key": "value"}
{"key": "value2"}
{"key": "value3"}"#;
        assert_json_stream!(data, data);
    }

    #[tokio::test]
    async fn test_json_stream_array() {
        let input = r#"[
{"key": "value"},
{"key": "value2"},
{"key": "value3"},"#;
        let output = r#"{"key": "value"}
{"key": "value2"}
{"key": "value3"}"#;
        assert_json_stream!(input, output);
    }
}
