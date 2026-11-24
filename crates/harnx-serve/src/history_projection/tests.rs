use super::*;
use crate::transcript_snapshot::HistorySnapshot;
use harnx_core::{message::MessageContentToolCalls, tool::ToolCall};
use serde_json::json;

const WRAPPED: &str = "<think>\nplanning\n</think>\n\nanswer";

#[test]
fn splits_consecutive_writer_wrappers_without_trimming_answer() {
    assert_eq!(
        split_history_reasoning(
            "<think>\nfirst\n</think>\n<think>\nsecond\n</think>\n\n\n answer  "
        ),
        ("\n answer  ", vec!["first", "second"])
    );
    assert_eq!(
        split_history_reasoning(
            "<think>\nplanning\n</think>\n<think>\nplanning\n</think>\n\nanswer"
        ),
        ("answer", vec!["planning"])
    );
    assert_eq!(
        split_history_reasoning(WRAPPED),
        ("answer", vec!["planning"])
    );
}

#[test]
fn thought_only_keeps_empty_assistant_content_and_keys_reasoning_by_wire_id() {
    let message = Message::new(
        MessageRole::Assistant,
        MessageContent::Text("<think>\n  planning\n</think>".into()),
    );
    let snapshot = HistorySnapshot::from_messages(&[message]);
    let [AgUiMessage::Assistant { id, content, .. }] = snapshot.messages.as_slice() else {
        panic!("expected one assistant row");
    };
    assert_eq!(content.as_deref(), Some(""));
    assert_eq!(
        snapshot.reasoning,
        BTreeMap::from([(id.to_string(), vec!["  planning".into()])])
    );
}

#[test]
fn literal_assistant_tags_and_non_assistant_text_are_unchanged() {
    for text in [
        "leading prose\n<think>\nx\n</think>\n\ntrailing prose",
        "```html\n<think>\nx\n</think>\n```",
        "<thinking>\nx\n</thinking>\nanswer",
        "<think>\nunclosed",
        "<think>inline</think>\nanswer",
        "<think>\nx\n</think>immediate literal",
        " <think>\nx\n</think>\nanswer",
    ] {
        assert_kept(MessageRole::Assistant, text);
    }
    for role in [MessageRole::System, MessageRole::User, MessageRole::Tool] {
        assert_kept(role, WRAPPED);
    }
}

fn assert_kept(role: MessageRole, text: &str) {
    let projected = project_message(&Message::new(role, MessageContent::Text(text.into())), 0);
    assert_eq!(projected.rows.len(), 1);
    assert_eq!(
        serde_json::to_value(&projected.rows[0]).unwrap()["content"],
        text
    );
    assert!(projected.reasoning.is_none());
}

#[test]
fn tool_rounds_split_prose_without_changing_calls_results_or_arguments() {
    let mut message = tool_round();
    let wrapped = project_message(&message, 0);
    let MessageContent::ToolCalls(calls) = &mut message.content else {
        unreachable!()
    };
    calls.text = "answer".into();
    let plain = project_message(&message, 0);
    assert_eq!(
        wrapped.rows.len(),
        2,
        "pending call must not have a result row"
    );
    assert_eq!(wrapped.rows[0], plain.rows[0]);
    assert_eq!(
        wrapped.reasoning,
        Some((plain.rows[0].id().to_string(), vec!["planning".into()]))
    );
    let assistant = serde_json::to_value(&wrapped.rows[0]).unwrap();
    assert_eq!(assistant["toolCalls"][0]["id"], "persisted-tool");
    assert_eq!(assistant["toolCalls"][1]["id"], "test-round-tool-1");
    assert_eq!(
        assistant["toolCalls"][0]["function"]["arguments"],
        json!({"input": WRAPPED}).to_string()
    );
    assert!(assistant["toolCalls"][0].get("thought_signature").is_none());
    let tool = serde_json::to_value(&wrapped.rows[1]).unwrap();
    assert_eq!(tool["toolCallId"], "persisted-tool");
    assert_eq!(tool["content"], WRAPPED);
}

fn tool_round() -> Message {
    let completed = ToolResult::new(
        ToolCall::new(
            "read".into(),
            json!({"input": WRAPPED}),
            Some("persisted-tool".into()),
            Some("provider-only signature".into()),
        ),
        json!(WRAPPED),
    );
    let pending = ToolResult::new(
        ToolCall::new("read".into(), json!({}), None, None),
        json!({"error": "tool response pending (results not yet persisted)"}),
    );
    let calls = MessageContentToolCalls::new(
        vec![completed, pending],
        WRAPPED.into(),
        Some("provider-only thought".into()),
    );
    Message::new(MessageRole::Tool, MessageContent::ToolCalls(calls)).with_id("test-round")
}
