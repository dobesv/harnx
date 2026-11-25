//! Map one durable transcript message to its AG-UI rows and optional reasoning.

use crate::{ag_ui_events::ag_ui_role_for_history, ag_ui_sync::wire_message_id};
use ag_ui_core::types::{
    ids::{MessageId, ToolCallId},
    message::{FunctionCall, Message as AgUiMessage},
    tool::ToolCall,
};
use harnx_core::{
    message::{Message, MessageContent, MessageRole},
    tool::ToolResult,
};
use std::collections::BTreeMap;

pub type HistoryReasoning = BTreeMap<String, Vec<String>>;

pub(crate) struct ProjectedMessage {
    pub rows: Vec<AgUiMessage>,
    pub reasoning: Option<(String, Vec<String>)>,
}

pub(crate) fn project_message(message: &Message, ordinal: usize) -> ProjectedMessage {
    let id = message
        .id
        .as_deref()
        .map(wire_message_id)
        .unwrap_or_else(MessageId::random);
    let (text, thoughts) = match &message.content {
        MessageContent::ToolCalls(calls) => split_history_reasoning(&calls.text),
        MessageContent::Text(text) if message.role == MessageRole::Assistant => {
            split_history_reasoning(text)
        }
        _ => {
            return ProjectedMessage {
                rows: vec![AgUiMessage::new(
                    ag_ui_role_for_history(message.role),
                    id,
                    message.content.to_text(),
                )],
                reasoning: None,
            };
        }
    };
    let reasoning = (!thoughts.is_empty()).then(|| {
        (
            id.to_string(),
            thoughts.into_iter().map(str::to_owned).collect(),
        )
    });
    let rows = match &message.content {
        MessageContent::ToolCalls(calls) => tool_rows(
            id,
            text,
            &calls.tool_results,
            &history_stable_base(message, ordinal),
        ),
        // The generic AG-UI constructor discards assistant text. Keep Some("")
        // even when reasoning accounted for the entire stored message.
        _ => vec![assistant_row(id, text, None)],
    };
    ProjectedMessage { rows, reasoning }
}

fn assistant_row(id: MessageId, text: &str, tool_calls: Option<Vec<ToolCall>>) -> AgUiMessage {
    AgUiMessage::Assistant {
        id,
        content: Some(text.to_owned()),
        name: None,
        tool_calls,
    }
}

fn tool_rows(id: MessageId, text: &str, results: &[ToolResult], base: &str) -> Vec<AgUiMessage> {
    let calls: Vec<_> = results
        .iter()
        .enumerate()
        .map(|(index, result)| history_tool_call(base, index, result))
        .collect();
    let completed = results
        .iter()
        .zip(&calls)
        .filter_map(|(result, call)| completed_tool_message(result, call.id.clone()))
        .collect::<Vec<_>>();
    let mut rows = vec![assistant_row(id, text, Some(calls))];
    rows.extend(completed);
    rows
}

fn history_tool_call(base: &str, index: usize, result: &ToolResult) -> ToolCall {
    // Never use the random wire message ID to synthesize a tool-call ID.
    let raw_id = result
        .call
        .id
        .clone()
        .unwrap_or_else(|| format!("{base}-tool-{index}"));
    ToolCall {
        id: serde_json::from_value(serde_json::Value::String(raw_id))
            .expect("tool call id should deserialize from string"),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: result.call.name.clone(),
            arguments: serde_json::to_string(&result.call.arguments)
                .expect("tool call args should serialize"),
        },
    }
}

fn history_stable_base(message: &Message, ordinal: usize) -> String {
    if let Some(id) = message.id.as_deref().filter(|id| !id.is_empty()) {
        return id.to_owned();
    }
    match message.log_seq {
        Some(seq) => format!("seq:{seq}"),
        None => format!("ord:{ordinal}"),
    }
}

fn completed_tool_message(result: &ToolResult, tool_call_id: ToolCallId) -> Option<AgUiMessage> {
    if harnx_runtime::config::session::is_pending_tool_result(result) {
        return None;
    }
    let content = result
        .markdown
        .clone()
        .or_else(|| result.output.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| result.output.to_string());
    Some(AgUiMessage::Tool {
        id: MessageId::random(),
        content,
        tool_call_id,
        error: None,
    })
}

/// Only recognize the leading display wrappers written by the model adapters
/// (two LFs after the closer) or session storage (one LF). Don't trim the
/// answer or recognize mid-text tags as the transcript renderers do.
pub(crate) fn split_history_reasoning(mut answer: &str) -> (&str, Vec<&str>) {
    let mut thoughts = Vec::new();
    while let Some((thought, rest)) = leading_thought(answer) {
        // Some stored messages contain both copies of the same writer wrapper.
        if !thought.is_empty() && thoughts.last() != Some(&thought) {
            thoughts.push(thought);
        }
        answer = rest;
    }
    (answer, thoughts)
}

fn leading_thought(text: &str) -> Option<(&str, &str)> {
    let (thought, tail) = text.strip_prefix("<think>\n")?.split_once("\n</think>")?;
    let answer = tail
        .strip_prefix("\n\n")
        .or_else(|| tail.strip_prefix('\n'))
        .or_else(|| tail.is_empty().then_some(tail))?;
    Some((thought, answer))
}

#[cfg(test)]
mod tests;
