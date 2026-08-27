use super::super::*;
use super::{assert_handoff_payload, collect_events};
use harnx_core::event::AgentEventSink;

fn find_custom_event<'a>(events: &'a [Event], name: &str) -> &'a CustomEvent {
    events
        .iter()
        .find_map(|event| match event {
            Event::Custom(custom) if custom.name == name => Some(custom),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} event should be present"))
}

#[test]
fn splits_requested_and_committed_handoff_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = AgUiSink::with_snapshot(tx, MessageId::random(), false, None);
    sink.emit(AgentEvent::Turn(TurnEvent::HandoffRequested {
        agent: "target-agent".to_string(),
        session_id: Some("target-session-123".to_string()),
    }));

    let Event::Custom(requested) = rx.try_recv().expect("handoff request custom event") else {
        panic!("expected custom handoff request event");
    };
    assert_eq!(requested.name, "turn_handoff_requested");
    assert_handoff_payload(&requested);
    assert!(
        rx.try_recv().is_err(),
        "an uncommitted request must not emit session_handoff"
    );

    sink.emit(AgentEvent::Session(SessionEvent::HandoffCommitted {
        agent: "target-agent".to_string(),
        session_id: "target-session-123".to_string(),
    }));
    sink.emit(AgentEvent::Turn(TurnEvent::Ended {
        outcome: harnx_core::event::TurnOutcome::default(),
    }));

    let events = collect_events(&mut rx);
    assert_eq!(events.len(), 1, "parent turn end adds no AG-UI step");
    assert_handoff_payload(find_custom_event(&events, "session_handoff"));
}

#[test]
fn nested_handoff_commit_does_not_navigate() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let sink = AgUiSink::with_snapshot(tx, MessageId::random(), false, None);
    sink.emit(AgentEvent::SubAgent {
        source: harnx_core::event::AgentSource::default(),
        event: Box::new(AgentEvent::Session(SessionEvent::HandoffCommitted {
            agent: "nested-target".to_string(),
            session_id: "nested-session".to_string(),
        })),
    });
    assert!(
        rx.try_recv().is_err(),
        "a nested handoff must not navigate the root Web session"
    );
}
