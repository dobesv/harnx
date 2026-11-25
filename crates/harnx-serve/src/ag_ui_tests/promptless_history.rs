use super::*;

const ANSWER: &str = "first assistant reply";
const THOUGHT: &str = "weighing options";

async fn seed_history(
    config: &Config,
    session_id: &str,
) -> (crate::session_actor::SessionRegistry, String) {
    let call_fn: AgentCallFn = Arc::new(|_input, _config, _abort| {
        Box::pin(async {
            for event in [
                ModelEvent::ThoughtChunk {
                    blocks: vec![ContentBlock::Text(THOUGHT.into())],
                },
                ModelEvent::MessageChunk {
                    blocks: vec![ContentBlock::Text(ANSWER.into())],
                },
            ] {
                harnx_core::sink::emit_agent_event(AgentEvent::Model(event));
            }
            let usage = CompletionTokenUsage::new(Some(1), Some(1), Some(0));
            Ok((ANSWER.into(), Some(THOUGHT.into()), vec![], usage))
        })
    });
    let registry = ag_ui_test_registry(config, Some(call_fn));
    let response = ag_ui_run_with_call_fn(
        config,
        &registry,
        "plain",
        session_id,
        &ag_ui_request_body(Uuid::new_v4(), "first user prompt"),
        None,
    )
    .await
    .expect("first run");
    let streamed = read_sse_events_until(response, |events| {
        events.iter().any(|event| event["type"] == "RUN_FINISHED")
    })
    .await;
    let live = streamed
        .iter()
        .filter(|event| event["type"] == "TEXT_MESSAGE_CONTENT")
        .map(|event| event["delta"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(live, ANSWER);
    (registry, live)
}

#[tokio::test]
async fn ag_ui_run_promptless_join_returns_persisted_history_in_snapshot() {
    // Issue #959: Subscribe must refresh history before sending its snapshot.
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "You are plain.");
    let config = sandbox.config();
    let session_id = "promptless-history-session";
    let (registry, live) = seed_history(&config, session_id).await;
    let join = json!({ "threadId": Uuid::new_v4(), "runId": Uuid::new_v4(), "messages": [] });
    let response = ag_ui_run_with_call_fn(
        &config,
        &registry,
        "plain",
        session_id,
        &serde_json::to_vec(&join).unwrap(),
        None,
    )
    .await
    .expect("promptless join");
    let events = read_sse_events_until(response, |events| events.len() >= 5).await;
    assert_promptless_frames(&events);
    assert_persisted_messages(
        events[3]["messages"].as_array().expect("messages array"),
        &live,
    );
}

fn assert_promptless_frames(events: &[Value]) {
    assert_event_type_sequence(
        events,
        &[
            "RUN_STARTED",
            "CUSTOM",
            "CUSTOM",
            "MESSAGES_SNAPSHOT",
            "RUN_FINISHED",
        ],
    );
    assert_eq!(events[1]["name"], "session_attach_boundary");
    assert_compaction_cleared(&events[2]);
    assert!(
        events[4].get("result").is_none(),
        "synthetic RUN_FINISHED must not have result key"
    );
}

fn assert_persisted_messages(messages: &[Value], live: &str) {
    assert!(
        messages.len() >= 2,
        "snapshot should have at least user + assistant"
    );
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .expect("user message");
    assert_eq!(user["content"], "first user prompt");
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant message");
    assert_eq!(assistant["content"], live);
}

#[test]
fn final_only_splits_reasoning_and_matches_projected_answer() {
    let output = "<think>\nweighing options\n</think>\n\nfirst assistant reply";
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = AgUiSink::with_snapshot(tx, MessageId::random(), false, None);
    emit_final(&sink, output);
    let events = collect_events(&mut rx);
    assert!(matches!(events.as_slice(), [
        Event::ThinkingStart(_),
        Event::ThinkingTextMessageStart(_),
        Event::ThinkingTextMessageContent(event),
        Event::ThinkingTextMessageEnd(_),
        Event::ThinkingEnd(_),
        Event::TextMessageStart(_),
        Event::TextMessageContent(text),
    ] if event.delta == THOUGHT && text.delta == ANSWER));
    let snapshot = HistorySnapshot::from_messages(&[Message::new(
        MessageRole::Assistant,
        MessageContent::Text(output.into()),
    )]);
    assert_eq!(
        serde_json::to_value(&snapshot.messages[0]).unwrap()["content"],
        ANSWER
    );
}

#[test]
fn final_does_not_repeat_streamed_reasoning_after_thinking_lifecycle_closed() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = AgUiSink::with_snapshot(tx, MessageId::random(), false, None);
    sink.emit(AgentEvent::Model(ModelEvent::ThoughtChunk {
        blocks: vec![ContentBlock::Text(THOUGHT.into())],
    }));
    sink.close_thinking_segment();
    assert!(matches!(
        collect_events(&mut rx).last(),
        Some(Event::ThinkingEnd(_))
    ));
    emit_final(
        &sink,
        "<think>\nweighing options\n</think>\nfirst assistant reply",
    );
    assert!(matches!(collect_events(&mut rx).as_slice(), [
        Event::TextMessageStart(_), Event::TextMessageContent(text)
    ] if text.delta == ANSWER));
}

#[test]
fn final_after_an_interrupted_turn_still_emits_reasoning() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = AgUiSink::with_snapshot(tx, MessageId::random(), false, None);
    sink.emit(AgentEvent::Turn(TurnEvent::Started));
    sink.emit(thought_chunk("interrupted thought"));
    sink.emit(AgentEvent::Turn(TurnEvent::Interrupted {
        cancellation_id: "cancel-first-turn".into(),
    }));
    sink.emit(AgentEvent::Turn(TurnEvent::Started));
    emit_final(&sink, "<think>\nnew thought\n</think>\nnew answer");
    assert_eq!(
        model_deltas(&mut rx),
        [
            ("thought", "interrupted thought".into()),
            ("thought", "new thought".into()),
            ("text", "new answer".into())
        ],
    );
}

#[test]
fn final_after_a_tool_round_still_emits_reasoning() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = AgUiSink::with_snapshot(tx, MessageId::random(), false, None);
    sink.emit(thought_chunk("before tool"));
    sink.emit(AgentEvent::Tool(ToolEvent::Started {
        id: "inspect".into(),
        name: "inspect".into(),
        kind: harnx_core::event::ToolKind::Other,
        markdown: None,
        input: json!({}),
        locations: vec![],
    }));
    sink.emit(AgentEvent::Tool(ToolEvent::Completed {
        id: "inspect".into(),
        output: json!("done"),
        markdown: None,
    }));
    emit_final(&sink, "<think>\nafter tool\n</think>\nanswer");
    assert_eq!(
        model_deltas(&mut rx),
        [
            ("thought", "before tool".into()),
            ("thought", "after tool".into()),
            ("text", "answer".into())
        ],
    );
}

#[test]
fn parent_and_sub_agent_finals_use_only_their_own_chunks() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = AgUiSink::with_snapshot(tx, MessageId::random(), false, None);
    let child = |event| {
        AgentEvent::sub_agent(
            harnx_core::event::AgentSource {
                agent: "child".into(),
                session_id: Some("child-session".into()),
                model: None,
            },
            event,
        )
    };

    // The child's Final must not consume the parent's streamed-thought state.
    sink.emit(thought_chunk("parent streamed"));
    sink.emit(child(final_event("<think>\nchild fallback\n</think>\n")));
    emit_final(&sink, "<think>\nparent streamed\n</think>\nparent answer");
    assert_eq!(
        model_deltas(&mut rx),
        [
            ("thought", "parent streamed".into()),
            ("thought", "child fallback".into()),
            ("text", "parent answer".into())
        ],
    );

    // The child's streamed chunks must not suppress a Final-only parent reply.
    sink.emit(child(thought_chunk("child streamed")));
    sink.emit(child(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("child answer".into())],
    })));
    emit_final(
        &sink,
        "<think>\nparent fallback\n</think>\nnew parent answer",
    );
    sink.emit(child(final_event(
        "<think>\nchild streamed\n</think>\nchild answer",
    )));
    assert_eq!(
        model_deltas(&mut rx),
        [
            ("thought", "child streamed".into()),
            ("text", "child answer".into()),
            ("thought", "parent fallback".into()),
            ("text", "new parent answer".into())
        ],
    );
}

fn model_deltas(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
) -> Vec<(&'static str, String)> {
    collect_events(rx)
        .into_iter()
        .filter_map(|event| match event {
            Event::ThinkingTextMessageContent(event) => Some(("thought", event.delta)),
            Event::TextMessageContent(event) => Some(("text", event.delta)),
            _ => None,
        })
        .collect()
}

fn thought_chunk(thought: &str) -> AgentEvent {
    AgentEvent::Model(ModelEvent::ThoughtChunk {
        blocks: vec![ContentBlock::Text(thought.into())],
    })
}

fn final_event(output: &str) -> AgentEvent {
    AgentEvent::Model(ModelEvent::Final {
        output: output.into(),
        usage: CompletionTokenUsage::default(),
    })
}

fn emit_final(sink: &AgUiSink, output: &str) {
    sink.emit(final_event(output));
}
