//! Chat Completions tool history, including provider-required reasoning replay.
use super::*;
use harnx_core::{message::MessageContentPart, tool::ToolResult};

pub(super) fn messages(calls: MessageContentToolCalls, model: &Model) -> Vec<Value> {
    let with_rounds = valid_rounds(&calls) && requires_reasoning_replay(model);
    let mut messages = if with_rounds {
        round_messages(&calls, model)
    } else if calls.sequence {
        calls
            .tool_results
            .iter()
            .flat_map(|result| {
                [
                    assistant_message(std::slice::from_ref(result)),
                    tool_message(result),
                ]
            })
            .collect()
    } else {
        let mut messages = vec![assistant_message(&calls.tool_results)];
        messages.extend(calls.tool_results.iter().map(tool_message));
        messages
    };
    if calls.rounds.is_empty() && !calls.sequence {
        replay_reasoning(&mut messages, model, &calls.text, calls.thought.as_deref());
    }
    if !with_rounds {
        append_images(&mut messages, &calls.tool_results);
    }
    messages
}

fn round_messages(calls: &MessageContentToolCalls, model: &Model) -> Vec<Value> {
    let mut offset: usize = 0;
    let mut messages = Vec::new();
    for round in &calls.rounds {
        let end = offset + round.tool_count;
        let results = &calls.tool_results[offset..end];
        let mut assistant = assistant_message(results);
        replay_reasoning(
            std::slice::from_mut(&mut assistant),
            model,
            &round.text,
            round.thought.as_deref(),
        );
        messages.push(assistant);
        messages.extend(results.iter().map(tool_message));
        append_images(&mut messages, results);
        offset = end;
    }
    messages
}

fn valid_rounds(calls: &MessageContentToolCalls) -> bool {
    !calls.rounds.is_empty()
        && calls.rounds.iter().all(|round| round.tool_count > 0)
        && calls
            .rounds
            .iter()
            .try_fold(0usize, |total, round| total.checked_add(round.tool_count))
            == Some(calls.tool_results.len())
}

fn append_images(messages: &mut Vec<Value>, results: &[ToolResult]) {
    let images = openai_collect_tool_result_images(results);
    if !images.is_empty() {
        messages.push(json!({"role": MessageRole::User, "content": images}));
    }
}

fn assistant_message(results: &[ToolResult]) -> Value {
    let calls: Vec<_> = results
        .iter()
        .map(|result| {
            json!({
                "id": result.call.id,
                "type": "function",
                "function": {
                    "name": result.call.name,
                    "arguments": result.call.arguments.to_string(),
                },
            })
        })
        .collect();
    json!({"role": MessageRole::Assistant, "tool_calls": calls})
}

fn tool_message(result: &ToolResult) -> Value {
    json!({
        "role": "tool",
        "content": openai_tool_message_content(result),
        "tool_call_id": result.call.id,
    })
}

/// These providers require the thinking accompanying a tool call on its
/// continuation. Other endpoints retain their existing request shape.
fn replay_reasoning(messages: &mut [Value], model: &Model, text: &str, thought: Option<&str>) {
    if !requires_reasoning_replay(model) {
        return;
    }
    for message in messages
        .iter_mut()
        .filter(|message| message["role"] == "assistant")
    {
        message["content"] = text.into();
        if let Some(thought) = thought {
            message["reasoning_content"] = thought.into();
        }
    }
}

pub(super) fn requires_reasoning_replay(model: &Model) -> bool {
    matches!(
        model.real_name(),
        "MiniMax-M3.1-Flash" | "glm-5.3" | "us.zai.glm-5.3"
    )
}

pub(super) fn streamed_text(handler: &mut SseHandler, data: &Value) -> Result<()> {
    let delta = &data["choices"][0]["delta"];
    if let Some(thought) = delta["reasoning_content"]
        .as_str()
        .or_else(|| delta["reasoning"].as_str())
    {
        handler.thought(thought)?;
    }
    if let Some(text) = delta["content"].as_str() {
        handler.text(text)?;
    }
    Ok(())
}

pub(super) fn split_output_reasoning(
    text: &str,
    reasoning: &str,
    model: &Model,
) -> (String, Option<String>) {
    let reasoning = if requires_reasoning_replay(model) {
        reasoning
    } else {
        reasoning.trim()
    };
    if reasoning.is_empty() {
        return (text.to_owned(), None);
    }
    if requires_reasoning_replay(model) {
        return (text.to_owned(), Some(reasoning.to_owned()));
    }
    (
        format!("<think>\n{}\n</think>\n\n{text}", reasoning.trim()),
        None,
    )
}

fn openai_collect_tool_result_images(tool_results: &[ToolResult]) -> Vec<Value> {
    tool_results
        .iter()
        .flat_map(|tool_result| tool_result.content.iter())
        .filter_map(|part| match part {
            MessageContentPart::ImageUrl { image_url } => Some(json!({
                "type": "image_url",
                "image_url": { "url": image_url.url },
            })),
            MessageContentPart::Text { .. } => None,
        })
        .collect()
}

fn openai_tool_message_content(tool_result: &ToolResult) -> String {
    let output = tool_result.output.to_string();
    if tool_result
        .content
        .iter()
        .any(|part| matches!(part, MessageContentPart::ImageUrl { .. }))
    {
        format!(
            "{output}\n[Tool {} returned image(s): see next message]",
            tool_result.call.name
        )
    } else {
        output
    }
}
