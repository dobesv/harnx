use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use harnx_core::abort::create_abort_signal;
use harnx_core::context::SessionCtx;
use harnx_core::event::{AgentEvent, AgentEventSink, NoticeEvent, TurnEvent};
use harnx_engine::{Engine, EngineInput};

/// Test-only sink that records every event into a shared Vec.
#[derive(Clone, Default)]
struct RecordingSink {
    events: Arc<Mutex<Vec<AgentEvent>>>,
}

impl RecordingSink {
    fn new() -> Self {
        Self::default()
    }

    fn snapshot(&self) -> Vec<AgentEvent> {
        self.events.lock().expect("sink mutex").clone()
    }
}

impl AgentEventSink for RecordingSink {
    fn emit(&self, event: AgentEvent, _source: Option<harnx_core::event::AgentSource>) {
        self.events.lock().expect("sink mutex").push(event);
    }
}

fn make_ctx(sink: Arc<dyn AgentEventSink>) -> Arc<SessionCtx> {
    Arc::new(SessionCtx::new(
        sink,
        create_abort_signal(),
        "test-session".into(),
        PathBuf::from("/tmp"),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_turn_emits_started_then_ended() {
    let sink = RecordingSink::new();
    let sink_arc: Arc<dyn AgentEventSink> = Arc::new(sink.clone());
    let ctx = make_ctx(sink_arc);

    let engine = Engine::new();
    let mut stream = Box::pin(engine.run_turn(ctx, EngineInput::new("hello")));

    let stream_events: Vec<AgentEvent> = {
        let mut collected = vec![];
        while let Some(ev) = stream.next().await {
            collected.push(ev);
        }
        collected
    };

    // Stream order: Started, Ended
    assert_eq!(stream_events.len(), 2, "unexpected event count in stream");
    assert!(matches!(
        stream_events[0],
        AgentEvent::Turn(TurnEvent::Started)
    ));
    assert!(matches!(
        stream_events[1],
        AgentEvent::Turn(TurnEvent::Ended { .. })
    ));

    // Sink got the same events.
    let sink_events = sink.snapshot();
    assert_eq!(
        sink_events.len(),
        stream_events.len(),
        "sink and stream should receive identical event counts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_turn_respects_preset_abort() {
    let sink = RecordingSink::new();
    let sink_arc: Arc<dyn AgentEventSink> = Arc::new(sink.clone());
    let ctx = make_ctx(sink_arc);
    ctx.abort.set_ctrlc();

    let engine = Engine::new();
    let mut stream = Box::pin(engine.run_turn(Arc::clone(&ctx), EngineInput::new("hello")));

    let mut events = vec![];
    while let Some(ev) = stream.next().await {
        events.push(ev);
    }

    // Expect: Started, Notice(Warning("interrupted")), Ended
    assert_eq!(events.len(), 3, "aborted turn should emit 3 events");
    assert!(matches!(events[0], AgentEvent::Turn(TurnEvent::Started)));
    assert!(matches!(
        events[1],
        AgentEvent::Notice(NoticeEvent::Warning(ref msg)) if msg == "interrupted"
    ));
    assert!(matches!(
        events[2],
        AgentEvent::Turn(TurnEvent::Ended { .. })
    ));

    // Sink observed the same events.
    assert_eq!(sink.snapshot().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_turn_stream_terminates_after_ended() {
    let sink: Arc<dyn AgentEventSink> = Arc::new(RecordingSink::new());
    let ctx = make_ctx(sink);

    let engine = Engine::new();
    let mut stream = Box::pin(engine.run_turn(ctx, EngineInput::default()));

    // Drain everything.
    let mut count = 0;
    while stream.next().await.is_some() {
        count += 1;
    }

    // The stream must have closed cleanly.
    assert!(count >= 2, "expected at least Started + Ended");
    assert!(stream.next().await.is_none(), "stream should stay closed");
}
