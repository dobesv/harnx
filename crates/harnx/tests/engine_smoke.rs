//! Cross-crate smoke test: harnx imports harnx-engine, constructs a
//! SessionCtx with the new CliAgentEventSink, drives Engine::run_turn,
//! and verifies events arrive on both the returned stream and the sink
//! via a proxy recording sink. Ensures the full dep chain
//! (harnx → harnx-engine → harnx-core) compiles and runs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use harnx_core::abort::create_abort_signal;
use harnx_core::context::SessionCtx;
use harnx_core::event::{AgentEvent, AgentEventSink, TurnEvent};
use harnx_engine::{Engine, EngineInput};

/// Proxy sink that records events in addition to forwarding them. The
/// real `CliAgentEventSink` writes to stderr — the integration test
/// needs to observe events programmatically, so we wrap it.
#[derive(Clone)]
struct ProxySink {
    inner: harnx::cli_event_sink::CliAgentEventSink,
    recorded: Arc<Mutex<Vec<AgentEvent>>>,
}

impl ProxySink {
    fn new() -> Self {
        Self {
            inner: harnx::cli_event_sink::CliAgentEventSink::new(
                false,
                harnx_render::RenderOptions::default(),
            ),
            recorded: Arc::new(Mutex::new(vec![])),
        }
    }

    fn snapshot(&self) -> Vec<AgentEvent> {
        self.recorded.lock().expect("proxy sink mutex").clone()
    }
}

impl AgentEventSink for ProxySink {
    fn emit(&self, event: AgentEvent, source: Option<harnx_core::event::AgentSource>) {
        self.recorded
            .lock()
            .expect("proxy sink mutex")
            .push(event.clone());
        self.inner.emit(event, source);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harnx_consumes_engine_stream_end_to_end() {
    let proxy = ProxySink::new();
    let sink_arc: Arc<dyn AgentEventSink> = Arc::new(proxy.clone());

    let ctx = Arc::new(SessionCtx::new(
        sink_arc,
        create_abort_signal(),
        "smoke-test".into(),
        PathBuf::from("/tmp"),
    ));

    let engine = Engine::new();
    let mut stream = Box::pin(engine.run_turn(Arc::clone(&ctx), EngineInput::new("hello")));

    let mut stream_events = vec![];
    while let Some(ev) = stream.next().await {
        stream_events.push(ev);
    }

    // Expect: Turn::Started, Turn::Ended.
    assert_eq!(
        stream_events.len(),
        2,
        "expected 2 events; got {stream_events:?}"
    );
    assert!(matches!(
        stream_events[0],
        AgentEvent::Turn(TurnEvent::Started)
    ));
    assert!(matches!(
        stream_events[1],
        AgentEvent::Turn(TurnEvent::Ended { .. })
    ));

    // Sink received the same events in the same order.
    let sink_events = proxy.snapshot();
    assert_eq!(sink_events.len(), 2);
    assert!(matches!(
        sink_events[0],
        AgentEvent::Turn(TurnEvent::Started)
    ));
    assert!(matches!(
        sink_events[1],
        AgentEvent::Turn(TurnEvent::Ended { .. })
    ));
}
