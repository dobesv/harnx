use super::*;
use crate::config::compaction::KeepPlan;
use harnx_core::{
    agent_config::AgentConfig,
    tool::{ToolCall, ToolResult},
};

fn completed_round(id: &str) -> ToolResult {
    ToolResult::new(
        ToolCall::new("read".into(), serde_json::json!({}), Some(id.into()), None),
        serde_json::json!({"text": "large output"}),
    )
}

fn continuation() -> (Session, Input) {
    let mut session = new(&Config::default(), "continuation", None).unwrap();
    attach_memory_log(&mut session);
    let mut input = Input::new(
        "original prompt".into(),
        ("original prompt".into(), vec![]),
        AgentConfig::from_prompt("system instructions"),
    );
    prepare_input(&mut session, &mut input).unwrap();
    let result = completed_round("dropped");
    session.messages.push(Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(crate::client::MessageContentToolCalls::new(
            vec![result.clone()],
            "working".into(),
            None,
        )),
    ));
    session.messages.push(Message::new(
        MessageRole::Assistant,
        MessageContent::Text("kept reply".into()),
    ));
    let input = input.merge_tool_results("working".into(), None, vec![result]);
    (session, input)
}

fn apply_plan(session: &mut Session, plan: KeepPlan) {
    CompactionLog::plan(session, "summary of dropped messages".into(), plan)
        .append()
        .apply(session);
}

#[test]
fn compaction_continuation_survives_prompt_only_empty_and_reply_only_kept_sets() {
    for plan in [
        KeepPlan {
            prompt_idx: Some(0),
            tail_from: 3,
        },
        KeepPlan {
            prompt_idx: None,
            tail_from: 3,
        },
        KeepPlan {
            prompt_idx: Some(0),
            tail_from: 2,
        },
    ] {
        let (mut session, mut input) = continuation();
        assert!(
            input.session_input_start.is_none(),
            "merge must discard previous cursor"
        );
        apply_plan(&mut session, plan);
        let kept = session.messages.clone();
        prepare_input(&mut session, &mut input).unwrap();
        assert_eq!(input.session_input_start, Some(kept.len()));
        assert_eq!(
            serde_json::to_value(&session.messages).unwrap(),
            serde_json::to_value(&kept).unwrap(),
            "preparation must not repeat original input"
        );
        assert_request_is_kept_history(&session, &input);
    }
}

fn assert_request_is_kept_history(session: &Session, input: &Input) {
    let request = build_messages(session, input).unwrap();
    let active: Vec<_> = request
        .into_iter()
        .filter(|m| {
            m.role != MessageRole::System
                && !m
                    .content
                    .to_text()
                    .contains("Earlier conversation summary:")
        })
        .collect();
    assert_eq!(
        serde_json::to_value(&active).unwrap(),
        serde_json::to_value(&session.messages).unwrap(),
        "archived tool results must stay archived"
    );
}

#[test]
fn compaction_rebases_non_contiguous_continuation_before_persisting_queued_input() {
    let (mut session, mut input) = continuation();
    let kept_result = completed_round("kept");
    session.messages.push(Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(crate::client::MessageContentToolCalls::new(
            vec![kept_result.clone()],
            "latest round".into(),
            None,
        )),
    ));
    input = input.merge_tool_results("latest round".into(), None, vec![kept_result]);
    apply_plan(
        &mut session,
        KeepPlan {
            prompt_idx: Some(0),
            tail_from: 3,
        },
    );
    input.set_injected_user_text("queued input".into());
    prepare_input(&mut session, &mut input).unwrap();
    assert_eq!(input.session_input_start, Some(2));
    // session::build_messages excludes the just-persisted injection; input's
    // outer builder adds its request-only patched form once.
    let request = build_messages(&session, &input).unwrap();
    assert_eq!(
        request
            .iter()
            .filter(|m| m.content.to_text() == "original prompt")
            .count(),
        1
    );
    let ids: Vec<_> = request
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::ToolCalls(calls) => calls.tool_results[0].call.id.as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["kept"]);
}
