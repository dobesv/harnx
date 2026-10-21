use super::*;
use harnx_core::event::{ContentBlock, ModelEvent, ToolEvent};

fn emit_round(sender: &dyn AgentEventSink) {
    sender.emit(AgentEvent::Tool(ToolEvent::Started {
        id: "round".into(),
        name: "read".into(),
        input: serde_json::json!({}),
        markdown: None,
        kind: harnx_core::event::ToolKind::Other,
        locations: vec![],
    }));
    sender.emit(AgentEvent::Tool(ToolEvent::Completed {
        id: "round".into(),
        output: serde_json::json!("done"),
        markdown: None,
    }));
    sender.emit(AgentEvent::Session(SessionEvent::CompactingStarted {
        compaction_id: None,
    }));
    sender.emit(completed());
    sender.emit(AgentEvent::Model(ModelEvent::MessageChunk {
        blocks: vec![ContentBlock::Text("Resumed answer".into())],
    }));
    sender.emit(AgentEvent::Model(ModelEvent::Final {
        output: "Resumed answer".into(),
        usage: Default::default(),
    }));
}

fn boundary(kind: &str) -> Event {
    serde_json::from_value(serde_json::json!({
        "type": kind, "threadId": uuid::Uuid::nil(), "runId": uuid::Uuid::nil(),
    }))
    .unwrap()
}

fn parse_frames(bytes: bytes::Bytes) -> Vec<serde_json::Value> {
    std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect()
}

fn wire_events(events: &mut broadcast::Receiver<Event>) -> Vec<serde_json::Value> {
    let mut guard = crate::ag_ui_lifecycle::LiveStreamGuard::default();
    let source = std::iter::once(boundary("RUN_STARTED"))
        .chain(std::iter::from_fn(|| events.try_recv().ok()));
    let mut wire: Vec<_> = source
        .filter_map(|event| guard.frame_event(event))
        .flat_map(parse_frames)
        .collect();
    wire.extend(parse_frames(guard.finalize_open_lifecycles()));
    wire.extend(parse_frames(
        guard.frame_event(boundary("RUN_FINISHED")).unwrap(),
    ));
    wire
}

fn assert_order(wire: &[serde_json::Value]) {
    let names: Vec<_> = wire
        .iter()
        .map(|event| {
            event
                .get("name")
                .unwrap_or(&event["type"])
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        names,
        [
            "RUN_STARTED",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_END",
            "TOOL_CALL_START",
            "TOOL_CALL_ARGS",
            "TOOL_CALL_END",
            "TOOL_CALL_RESULT",
            "session_compacting_started",
            "session_compacting_completed",
            "session_compaction",
            "MESSAGES_SNAPSHOT",
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_END",
            "RUN_FINISHED",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_turn_compaction_snapshot_precedes_resumed_stream_and_excludes_future_history() {
    harnx_core::require_nextest();
    let sandbox = TestConfigSandbox::new();
    sandbox.write_agent("plain", "Read the compacted history.");
    let mut config = sandbox.config();
    let Some(key) = seed(&config, 23).await else {
        return;
    };
    // The durable reader already has the later prompt/reply, but their live
    // events haven't crossed the relay yet. Snapshot must stop at this boundary.
    config.session = Some(harnx_core::session::Session {
        log_entry_count: 19,
        ..Default::default()
    });
    let (sender, relay, mut events) = relay(config, key);
    relay
        .run(Box::pin(async move {
            emit_round(sender.as_ref());
        }))
        .await;
    let wire = wire_events(&mut events);
    assert_order(&wire);
    let snapshot = &wire
        .iter()
        .find(|event| event["type"] == "MESSAGES_SNAPSHOT")
        .unwrap()["messages"];
    assert_eq!(
        snapshot.as_array().unwrap().len(),
        expected(19).messages.len()
    );
    assert!(!snapshot
        .to_string()
        .contains("Show the previous compaction's history."));
}
