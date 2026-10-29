//! Buffer the terminal response for callers that do not consume streamed chunks.
//! Parsing the terminal object preserves IDs, reasoning, tool signatures and usage
//! without emitting duplicate chunk events to a non-streaming caller's sink.
use crate::*;
use anyhow::{Context, Result};
use serde_json::Value;

pub(super) async fn completion(
    builder: reqwest::RequestBuilder,
    model: &Model,
) -> Result<ChatCompletionsOutput> {
    let mut terminal = None;
    crate::sse_stream_with_content_type(
        builder,
        |message| {
            let data: Value =
                serde_json::from_str(&message.data).context("Invalid Codex response event")?;
            harnx_core::llm_trace::stream_event("openai_responses", &data);
            let kind = match message.event.as_str() {
                "" | "message" => data["type"].as_str().unwrap_or_default(),
                kind => kind,
            };
            match kind {
                "response.completed" | "response.incomplete" => {
                    terminal = Some(data["response"].clone());
                    Ok(true)
                }
                "response.failed" | "response.error" | "error" => {
                    crate::openai_responses::responses_stream_error(&data)?;
                    Ok(false)
                }
                _ => Ok(false),
            }
        },
        SseContentType::AllowMissing,
    )
    .await?;
    let response = terminal.context("Codex response stream ended before a terminal response")?;
    crate::openai_responses::openai_extract_responses(&response, model)
}

#[cfg(test)]
mod tests;
