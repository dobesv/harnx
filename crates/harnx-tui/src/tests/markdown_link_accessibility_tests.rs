use super::*;

#[path = "intermediate_assistant_links.rs"]
mod intermediate_assistant_links;
use crate::lifecycle::messages_to_transcript_items;
use crate::subagent_transcript::apply_child_event;
use crate::types::{MonitoredSessionState, SubAgentStatus};
use harnx_core::message::{Message, MessageContent, MessageContentToolCalls, MessageRole};
use harnx_core::tool::{ToolCall, ToolResult};
use harnx_runtime::tool::ToolDeclaration;
use serde_json::{json, Value};
use std::collections::HashMap;

fn started(id: &str, markdown: &str) -> AgentEvent {
    AgentEvent::Tool(ToolEvent::Started {
        id: id.into(),
        name: "lookup".into(),
        kind: ToolKind::Other,
        markdown: Some(markdown.into()),
        input: Value::Null,
        locations: vec![],
    })
}

fn updated(id: &str, markdown: Option<&str>) -> AgentEvent {
    AgentEvent::Tool(ToolEvent::Update {
        id: id.into(),
        markdown: markdown.map(str::to_string),
        status: Some(ToolStatus::InProgress),
        content: None,
        title: None,
        kind: None,
        locations: None,
        usage: None,
    })
}

fn completed(id: &str, output: Value, markdown: Option<String>) -> AgentEvent {
    AgentEvent::Tool(ToolEvent::Completed {
        id: id.into(),
        output,
        markdown,
    })
}

// Compare content and order, ignoring timestamps, IDs and live timer metadata.
pub(super) fn rows(items: &[TranscriptItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            TranscriptItem::ToolCall { tool_name, .. } => format!("call:{tool_name}"),
            TranscriptItem::ToolResultMarkdown {
                text, full_detail, ..
            } => {
                format!("result:{text}\ndetail:{full_detail:?}")
            }
            TranscriptItem::MarkdownLink { text, url } => format!("link:{text}:{url}"),
            TranscriptItem::SubAgentSession { key, .. } => {
                format!("status:{}/{}", key.agent, key.session_id)
            }
            other => panic!("unexpected row: {other:?}"),
        })
        .collect()
}

fn update_sequence() -> Vec<(AgentEvent, Vec<&'static str>)> {
    let replacement = "[One](https://one.example) [Two](cid:two) [Duplicate](https://one.example)";
    let old = vec!["call:lookup", "link:Old:https://old.example"];
    let other = vec!["call:lookup", "link:Other:https://other.example"];
    let result = vec![
        "result:[Result](https://result.example)\ndetail:None",
        "link:Result:https://result.example",
    ];
    let original = [old.clone(), other.clone(), result.clone()].concat();
    let replaced = [
        vec![
            "call:lookup",
            "link:One:https://one.example",
            "link:Two:cid:two",
        ],
        other.clone(),
        result.clone(),
    ]
    .concat();
    let removed = [vec!["call:lookup"], other.clone(), result].concat();
    let done = [removed.clone(), vec!["result:done\ndetail:None"]].concat();
    vec![
        (started("a", "[Old](https://old.example)"), old.clone()),
        (
            started("b", "[Other](https://other.example)"),
            [old, other].concat(),
        ),
        (
            completed("b", json!("[Result](https://result.example)"), None),
            original,
        ),
        (updated("a", Some(replacement)), replaced.clone()),
        (updated("a", Some(replacement)), replaced.clone()),
        (updated("a", None), replaced),
        (updated("a", Some("No links now")), removed),
        (completed("a", json!("done"), None), done.clone()),
        (updated("a", Some("[Late](https://late.example)")), done),
    ]
}

#[tokio::test]
async fn main_updates_replace_only_owned_markdown_links() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    for (event, expected) in update_sequence() {
        tui.handle_tui_event(TuiEvent::LocalAgent(event))
            .await
            .unwrap();
        assert_eq!(rows(&tui.app.transcript), expected);
    }
}

#[test]
fn child_updates_replace_only_owned_markdown_links() {
    let mut state = MonitoredSessionState::new(SubAgentStatus::Running);
    for (event, expected) in update_sequence() {
        apply_child_event(&mut state, event);
        assert_eq!(rows(&state.transcript), expected);
    }
}

#[tokio::test]
async fn main_update_fallbacks_extract_links_and_remain_idempotent() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    for idless in [false, true] {
        tui.clear_transcript();
        if idless {
            tui.render_agent_event(started("a", "[Old](https://old.example)"))
                .await;
            if let TranscriptItem::ToolCall { id, .. } = &mut tui.app.transcript[0] {
                *id = None;
            }
        }
        let event = updated("a", Some("[New](https://new.example)"));
        tui.render_agent_event(event.clone()).await;
        tui.render_agent_event(event).await;
        assert_eq!(
            rows(&tui.app.transcript),
            vec![
                if idless { "call:lookup" } else { "call:tool" },
                "link:New:https://new.example"
            ]
        );
    }
}

#[test]
fn child_update_fallbacks_extract_links_and_remain_idempotent() {
    for idless in [false, true] {
        let mut state = MonitoredSessionState::new(SubAgentStatus::Running);
        if idless {
            apply_child_event(&mut state, started("a", "[Old](https://old.example)"));
            if let TranscriptItem::ToolCall { id, .. } = &mut state.transcript[0] {
                *id = None;
            }
        }
        let event = updated("a", Some("[New](https://new.example)"));
        apply_child_event(&mut state, event.clone());
        apply_child_event(&mut state, event);
        assert_eq!(
            rows(&state.transcript),
            vec![
                if idless { "call:lookup" } else { "call:tool" },
                "link:New:https://new.example"
            ]
        );
    }
}

fn history(
    output: Value,
    call_template: Option<String>,
    result_template: Option<String>,
) -> Vec<TranscriptItem> {
    let declaration = ToolDeclaration {
        name: "lookup".into(),
        description: String::new(),
        parameters: Default::default(),
        mcp_tool_name: None,
        mcp_server_name: None,
        call_template,
        result_template,
        idempotent_hint: None,
        read_only_hint: None,
        kind: None,
    };
    let call = ToolCall::new("lookup".into(), Value::Null, Some("a".into()), None);
    let messages = vec![Message::new(
        MessageRole::Tool,
        MessageContent::ToolCalls(MessageContentToolCalls::new(
            vec![ToolResult::new(call, output)],
            String::new(),
            None,
        )),
    )];
    messages_to_transcript_items(&messages, &HashMap::from([("lookup".into(), declaration)]))
}

fn middle_link_template() -> String {
    let defaults = harnx_core::safety::TruncateOpts::default();
    let terminal_head = crossterm::terminal::size().map_or(0, |(_, rows)| usize::from(rows / 2));
    // Non-TTY previews keep a tail (75 lines by default), while TTY previews
    // keep only a head sized by terminal height. Put both links in the omitted
    // middle for either policy, even on a very tall terminal.
    let head = "padding\n".repeat(defaults.head_lines.max(terminal_head).max(5) + 1);
    let tail = "padding\n".repeat(defaults.tail_lines + 1);
    format!("{head}\n[First](https://{{{{ result.host }}}}/docs) [Second][document]\n\n[document]: cid:document\n{tail}")
}

#[tokio::test]
async fn structured_result_templates_extract_untruncated_links_with_history_live_child_parity() {
    let call_markdown = "[Prompt](https://prompt.example)";
    let template = middle_link_template();
    let rendered = template.replace("{{ result.host }}", "template.example");
    let output = json!({"host": "template.example", "raw": "[Not displayed](https://raw.example)"});
    let historical = history(output.clone(), Some(call_markdown.into()), Some(template));
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    tui.render_agent_event(started("a", call_markdown)).await;
    tui.render_agent_event(completed("a", output.clone(), Some(rendered.clone())))
        .await;
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    apply_child_event(&mut child, started("a", call_markdown));
    apply_child_event(&mut child, completed("a", output, Some(rendered)));
    assert_eq!(rows(&tui.app.transcript), rows(&historical));
    assert_eq!(rows(&child.transcript), rows(&historical));
    assert!(
        matches!(historical.as_slice(), [TranscriptItem::ToolCall { .. }, TranscriptItem::MarkdownLink { .. }, TranscriptItem::ToolResultMarkdown { text, .. }, TranscriptItem::MarkdownLink { text: first, url: first_url }, TranscriptItem::MarkdownLink { text: second, url: second_url }] if !text.contains("First") && first == "First" && first_url == "https://template.example/docs" && second == "Second" && second_url == "cid:document"),
        "{}",
        rows(&historical).join("\n")
    );
}

#[tokio::test]
async fn full_results_keep_hidden_links_and_detail_pairing_across_call_links() {
    let output = json!({"content": [
        {"type": "text", "text": "Summary", "annotations": {"audience": ["user"]}},
        {"type": "text", "text": "[Hidden](cid:hidden)", "annotations": {"audience": ["assistant"]}}
    ]});
    let call = "[Prompt](cid:prompt) [Second](cid:second)";
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    tui.render_agent_event(started("a", call)).await;
    tui.render_agent_event(completed("a", output.clone(), None))
        .await;
    let historical = history(output.clone(), Some(call.into()), None);
    assert_eq!(rows(&tui.app.transcript), rows(&historical));
    assert!(
        matches!(&historical[4], TranscriptItem::MarkdownLink { url, .. } if url == "cid:hidden")
    );
    let mut child = MonitoredSessionState::new(SubAgentStatus::Running);
    apply_child_event(&mut child, started("a", call));
    apply_child_event(&mut child, completed("a", output, None));
    assert_eq!(rows(&child.transcript), rows(&historical));

    // Single-call focus, call + its link rows, and selection including the result.
    // All must include full detail exactly once.
    for end in [0, 2, 3] {
        tui.app.transcript_selection_anchor = Some(0);
        tui.app.transcript_focus = Some(end);
        let (entries, _) = crate::detail_view::detail_view_content(&tui.app);
        let detail = entries
            .iter()
            .flatten()
            .map(line_to_plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            detail.matches("[Hidden](cid:hidden)").count(),
            1,
            "selection end {end}: {detail}"
        );
    }
    tui.app.transcript_selection_anchor = None;
    tui.app.transcript_focus = Some(2);
    tui.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        tui.app.transcript_focus,
        Some(4),
        "result stays non-navigable, its URL row stays navigable"
    );
}

#[test]
fn blank_result_template_falls_back_to_full_output_links() {
    let output = json!(format!(
        "{}\n[Last](https://last.example)",
        "padding\n".repeat(100)
    ));
    let absent = crate::tool_transcript::tool_completed_to_transcript_items(&output, None);
    let blank = crate::tool_transcript::tool_completed_to_transcript_items(&output, Some(" \n "));
    assert_eq!(rows(&absent), rows(&blank));
    assert!(
        matches!(blank.last(), Some(TranscriptItem::MarkdownLink { url, .. }) if url == "https://last.example")
    );
}

#[tokio::test]
async fn blocked_markdown_body_exposes_reason_links() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    tui.clear_transcript();
    tui.render_agent_event(AgentEvent::Tool(ToolEvent::Blocked {
        id: "blocked".into(),
        name: "lookup".into(),
        input: Value::Null,
        reason: "See [Policy](cid:policy)".into(),
    }))
    .await;
    assert_eq!(
        rows(&tui.app.transcript),
        vec!["call:lookup", "link:Policy:cid:policy"]
    );
}

fn message_rows(items: &[TranscriptItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            TranscriptItem::UserText { text, .. } => format!("user:{text}"),
            TranscriptItem::AssistantText { text, .. } => format!("assistant:{text}"),
            TranscriptItem::SystemText(text) => format!("system:{text}"),
            TranscriptItem::MarkdownLink { text, url } => format!("link:{text}:{url}"),
            other => panic!("unexpected row: {other:?}"),
        })
        .collect()
}

async fn seed_non_tail_stream(tui: &mut Tui) {
    tui.clear_transcript();
    tui.render_agent_event(AgentEvent::User(UserEvent::Message {
        content: "question".into(),
    }))
    .await;
    tui.render_agent_event(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("partial reply".into())],
    }))
    .await;
    tui.render_agent_event(AgentEvent::Notice(NoticeEvent::Info("later status".into())))
        .await;
    tui.app.transcript.push(TranscriptItem::AssistantText {
        text: "later child reply".into(),
        seq: None,
        timestamp: None,
        rendered_cache: None,
    });
    assert_eq!(tui.app.main_streamed_text_idx, Some(1));
}

#[tokio::test]
async fn streamed_non_tail_reply_keeps_links_and_selection_targets() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    let linked = "[First](https://first.example) [Second](cid:second)";
    for output in [linked, "plain reply"] {
        for (focus, anchor) in [
            (Some(3), Some(2)),
            (Some(2), Some(3)),
            (Some(1), Some(0)),
            (Some(0), Some(1)),
            (None, None),
        ] {
            seed_non_tail_stream(&mut tui).await;
            tui.app.transcript_focus = focus;
            tui.app.transcript_selection_anchor = anchor;
            let timestamp = match &tui.app.transcript[1] {
                TranscriptItem::AssistantText { timestamp, .. } => *timestamp,
                _ => unreachable!(),
            };
            tui.render_agent_event(AgentEvent::Model(ModelEvent::Final {
                output: output.into(),
                usage: Default::default(),
            }))
            .await;
            let expected_reply = messages_to_transcript_items(
                &[Message::new(
                    MessageRole::Assistant,
                    MessageContent::Text(output.into()),
                )],
                &HashMap::new(),
            );
            let mut expected = vec!["user:question".into()];
            expected.extend(message_rows(&expected_reply));
            expected.extend([
                "system:later status".into(),
                "assistant:later child reply".into(),
            ]);
            assert_eq!(message_rows(&tui.app.transcript), expected);
            let links = expected_reply.len() - 1;
            let shifted = |index: Option<usize>| index.map(|i| if i > 1 { i + links } else { i });
            assert_eq!(tui.app.transcript_focus, shifted(focus));
            assert_eq!(tui.app.transcript_selection_anchor, shifted(anchor));
            assert!(
                matches!(&tui.app.transcript[1], TranscriptItem::AssistantText { timestamp: actual, rendered_cache: None, .. } if actual == &timestamp)
            );
            assert!(!tui.app.streaming_open);
            assert_eq!(tui.app.main_streamed_text_idx, None);
        }
    }
}

#[tokio::test]
async fn final_reply_fallback_appends_adjacent_links_without_shifting_selection() {
    let mut tui = Tui::init(&test_config()).await.unwrap();
    let output = "[First](https://first.example) [Second](cid:second)";
    for streamed_idx in [None, Some(0), Some(usize::MAX)] {
        tui.clear_transcript();
        tui.app
            .transcript
            .push(TranscriptItem::SystemText("existing status".into()));
        tui.app.main_streamed_text_idx = streamed_idx;
        tui.app.transcript_focus = Some(0);
        tui.app.transcript_selection_anchor = Some(0);
        tui.render_agent_event(AgentEvent::Model(ModelEvent::Final {
            output: output.into(),
            usage: Default::default(),
        }))
        .await;
        assert_eq!(
            message_rows(&tui.app.transcript),
            vec![
                "system:existing status".into(),
                format!("assistant:{output}"),
                "link:First:https://first.example".into(),
                "link:Second:cid:second".into(),
            ]
        );
        assert_eq!(tui.app.transcript_focus, Some(0));
        assert_eq!(tui.app.transcript_selection_anchor, Some(0));
    }
}

async fn queue_linked_prompt(tui: &mut Tui, text: &str, attachment: &crate::types::Attachment) {
    tui.clear_transcript();
    tui.app.llm_busy = true;
    tui.app.attachments = vec![attachment.clone()];
    tui.set_input_text(text);
    tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(tui.app.pending_message.is_some());
    assert!(
        tui.app.transcript.is_empty(),
        "queued draft is not rendered until submitted"
    );
    tui.complete_main_prompt().await;
    assert!(tui.app.pending_message.is_none());
}

#[tokio::test]
async fn queued_prompt_links_are_adjacent_and_match_normal_and_history() {
    let _lock = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let _env = TestEnvironment::set(dir.path());
    let config = test_config_with_mock_client_and_agent("test-agent", Some("links-parity"));
    let mut tui = Tui::init(&config).await.unwrap();
    let path = dir.path().join("note.txt");
    std::fs::write(&path, "attachment preview").unwrap();
    let attachment = crate::types::Attachment {
        path,
        display_name: "note.txt".into(),
    };
    for text in ["See [First](https://first.example) [Second](cid:second) [Duplicate](https://first.example)", ".echo [First](https://first.example) [Second](cid:second)"] {
        queue_linked_prompt(&mut tui, text, &attachment).await;
        let history = messages_to_transcript_items(&[Message::new(MessageRole::User, MessageContent::Text(text.into()))], &HashMap::new());
        let expected = vec![format!("user:{text}"), "link:First:https://first.example".into(), "link:Second:cid:second".into()];
        assert_eq!(message_rows(&history), expected);
        assert_eq!(message_rows(&tui.app.transcript[..3]), expected);
        assert!(matches!(&tui.app.transcript[3], TranscriptItem::AttachmentHeader(text) if text == "Attachments (1)"));
        assert!(matches!(&tui.app.transcript[4], TranscriptItem::AttachmentItem(name) if name == "note.txt"));
        assert!(matches!(&tui.app.transcript[5], TranscriptItem::AttachmentPreviewLine(line) if line == "attachment preview"));
        tui.retire_prompt_task();
        tui.clear_transcript();
        tui.app.llm_busy = false;
        tui.app.attachments = vec![attachment.clone()];
        tui.set_input_text(text);
        tui.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await.unwrap();
        assert_eq!(message_rows(&tui.app.transcript[..3]), expected);
        tui.retire_prompt_task();
        tui.clear_transcript();
        tui.finish_durable_pending_enqueue(crate::types::PendingMessage { text: text.into(), attachments: vec![], attachment_dir: None, paste_count: 0 }).await;
        assert_eq!(message_rows(&tui.app.transcript), expected);
    }
}
