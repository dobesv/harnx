use super::*;
use crate::nats_event_sink::{AdvisoryEnvelope, LiveEventState};
use harnx_core::event::{ToolEvent, TurnEvent};

#[derive(Default)]
struct Capture(std::sync::Mutex<Vec<AgentEvent>>);
impl AgentEventSink for Capture {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[test]
fn final_flush_drops_stopped_and_replaced_generations_before_any_decoration() {
    let live = LiveEventState::default();
    live.select(Some("g1".into()));
    let capture = Arc::new(Capture::default());
    let sink: Arc<dyn AgentEventSink> = capture.clone();
    let events = vec![
        AgentEvent::Model(ModelEvent::Final {
            output: "old".into(),
            usage: Default::default(),
        }),
        AgentEvent::Tool(ToolEvent::Completed {
            id: "call".into(),
            output: "old".into(),
            markdown: None,
        }),
        AgentEvent::Tool(ToolEvent::Progress {
            id: "call".into(),
            text: "old".into(),
        }),
        AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }),
    ];
    let queued: VecDeque<_> = events
        .into_iter()
        .map(|event| AdvisoryEnvelope::new(u64::MAX, event).with_execution_id("g1"))
        .collect();
    let mut seqs = HashSet::new();
    live.stop("g1");
    for generation in ["g1", "g2"] {
        live.select(Some(generation.into()));
        for mode in [AdvisoryFlush::Live, AdvisoryFlush::Final] {
            let mut pending = queued.clone();
            flush_pending_advisories(&mut pending, None, &sink, &mut seqs, mode, &live);
            assert!(pending.is_empty());
            assert!(capture.0.lock().unwrap().is_empty());
            assert!(
                seqs.is_empty(),
                "stale event must not assign a current transcript seq"
            );
        }
    }
}

#[test]
fn historical_replay_still_renders_committed_old_output() {
    let capture = Arc::new(Capture::default());
    let history = vec![(
        1,
        SessionLogEntry::Message {
            id: Some("old".into()),
            role: harnx_core::message::MessageRole::Assistant,
            content: MessageContent::Text("committed before stop".into()),
            timestamp: None,
            fence_token: None,
        },
    )];
    replay_entries_to_sink(&history, capture.clone());
    assert!(capture.0.lock().unwrap().iter().any(|event| matches!(event,
        AgentEvent::Model(ModelEvent::Final { output, .. }) if output == "committed before stop")));
}

#[test]
fn partial_assistant_does_not_complete_before_durable_error() {
    let mut entries = vec![
        (
            1,
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::User,
                content: MessageContent::Text("prompt".into()),
                timestamp: None,
                fence_token: None,
            },
        ),
        (
            2,
            SessionLogEntry::Message {
                id: None,
                role: MessageRole::Assistant,
                content: MessageContent::Text(String::new()),
                timestamp: None,
                fence_token: Some(1),
            },
        ),
    ];
    assert!(!NatsSession::is_turn_completion_visible(&entries, None, 1));
    entries.push((
        3,
        SessionLogEntry::Error {
            message: "terminal model failure".into(),
            fence_token: 1,
            timestamp: None,
        },
    ));
    assert!(NatsSession::is_turn_completion_visible(&entries, None, 1));
    assert_eq!(
        NatsSession::extract_turn_error(&entries, 1).as_deref(),
        Some("terminal model failure")
    );
}
