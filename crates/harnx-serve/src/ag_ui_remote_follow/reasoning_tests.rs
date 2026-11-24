use super::*;
use crate::{
    ag_ui_lifecycle::LiveStreamGuard, test_support::TestConfigSandbox,
    transcript_snapshot::HistorySnapshot,
};
use harnx_core::{
    message::{MessageContent, MessageRole},
    session::Session,
};
use harnx_runtime::nats_event_sink::LiveEventState;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio_stream::StreamExt;

const THOUGHT: &str = "check the durable reply";
const ANSWER: &str = "done";

#[tokio::test]
async fn queued_remote_snapshot_and_reasoning_share_one_fenced_batch() {
    let [_, snapshot] = HistorySnapshot::from_entries(&reply_history(), &Session::default(), false)
        .unwrap()
        .live_events();
    for opted_in in [false, true] {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        // The older snapshot and its reasoning must both be dropped.
        for after_seq in [1, 2] {
            tx.try_send(QueuedEvent {
                after_seq,
                event: snapshot.clone(),
            })
            .unwrap();
        }
        drop(tx);
        let frames = event_frames_with_guard(
            rx,
            LiveEventState::default(),
            2,
            Arc::new(Mutex::new(LiveStreamGuard::default())),
            features(opted_in),
        )
        .collect::<Vec<_>>()
        .await;
        assert_eq!(frames.len(), 1, "snapshot and companion must share a batch");
        assert_reasoning_frames(&decode_frames(&frames), opted_in);
    }
}

#[tokio::test]
async fn remote_attach_completed_before_follow_hydrates_opted_in_reasoning() {
    let sandbox = TestConfigSandbox::new();
    let config = sandbox.config();
    let history = reply_history();
    for opted_in in [false, true] {
        let stream = complete_remote_attach(
            RemoteEventStreamParams {
                config: &config,
                cluster: "local",
                session_id: "remote-reasoning",
                run_id: "run",
                thread_id: "thread",
                snapshot_frame: None,
                attachment_frames: vec![],
                session_base: Some(Session::default()),
                features: features(opted_in),
            },
            RemoteFollowTerminal::Finished,
            &history,
            [Bytes::new(), Bytes::new()],
        )
        .unwrap();
        let events = decode_frames(&stream.collect::<Vec<_>>().await);
        assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
        assert_reasoning_frames(&events, opted_in);
    }
}

fn features(opted_in: bool) -> SnapshotFeatures {
    if opted_in {
        SnapshotFeatures::WITH_HISTORY_REASONING
    } else {
        SnapshotFeatures::default()
    }
}

fn reply_history() -> Vec<(u64, SessionLogEntry)> {
    vec![(
        2,
        SessionLogEntry::Message {
            id: Some("remote-assistant".into()),
            role: MessageRole::Assistant,
            content: MessageContent::Text(format!("<think>\n{THOUGHT}\n</think>\n{ANSWER}")),
            timestamp: None,
            fence_token: None,
        },
    )]
}

fn decode_frames(frames: &[Bytes]) -> Vec<Value> {
    frames
        .iter()
        .flat_map(|frame| std::str::from_utf8(frame).unwrap().lines())
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect()
}

fn assert_reasoning_frames(events: &[Value], opted_in: bool) {
    let snapshots = events
        .iter()
        .filter(|e| e["type"] == "MESSAGES_SNAPSHOT")
        .count();
    assert_eq!(snapshots, 1);
    let snapshot_at = events
        .iter()
        .position(|e| e["type"] == "MESSAGES_SNAPSHOT")
        .unwrap();
    let snapshot = &events[snapshot_at];
    assert!(
        snapshot.get("rawEvent").is_none(),
        "internal reasoning must never be sent in rawEvent"
    );
    let [row] = snapshot["messages"].as_array().unwrap().as_slice() else {
        panic!("expected one row in snapshot: {snapshot}");
    };
    assert_eq!(row["role"], "assistant");
    assert_eq!(row["content"], ANSWER);
    let companions = events
        .iter()
        .filter(|e| e["name"] == "history_reasoning")
        .count();
    assert_eq!(companions, usize::from(opted_in));
    if opted_in {
        let id = row["id"].as_str().unwrap();
        assert_eq!(
            events[snapshot_at + 1],
            json!({
                "type": "CUSTOM", "name": "history_reasoning", "value": { (id): [THOUGHT] },
            })
        );
    }
}
