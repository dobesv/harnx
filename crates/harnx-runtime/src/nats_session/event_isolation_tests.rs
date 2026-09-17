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
fn final_flush_drops_events_at_or_before_the_cancel_sequence_before_any_decoration() {
    let live = LiveEventState::default();
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
    // Every event here predates the cancel fence set below (seq 5 < seq 10).
    let queued: VecDeque<_> = events
        .into_iter()
        .map(|event| AdvisoryEnvelope::new(5, event))
        .collect();
    let mut seqs = HashSet::new();
    live.accept_interrupt(10);
    for mode in [AdvisoryFlush::Live, AdvisoryFlush::Final] {
        let mut pending = queued.clone();
        flush_pending_advisories(&mut pending, None, &sink, &mut seqs, mode, &live, 0);
        assert!(pending.is_empty());
        assert!(capture.0.lock().unwrap().is_empty());
        assert!(
            seqs.is_empty(),
            "stale event must not assign a current transcript seq"
        );
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
