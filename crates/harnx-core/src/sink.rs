//! Process-level `AgentEventSink` registry. Any code that generates
//! user-visible events can call `emit_agent_event` and have the active
//! sink (TUI, CLI, or none) render it. Lives in harnx-core so both
//! harnx and harnx-engine can reach it without cycles.
//!
//! Uses a `Mutex<Option<...>>` so installers can overwrite each other
//! (needed for cross-crate `#[cfg(test)]` callers: harnx's tests must
//! be able to install/clear sinks even though harnx-core is compiled
//! as a non-test dep). The uncontended-lock cost per emit is
//! negligible compared to rendering.
//!
//! Events emitted before any sink is installed (e.g. MCP connection
//! warnings raised during `agent::init`, which runs before the TUI/CLI
//! sink is wired up) are held in a small bounded buffer and replayed
//! to the first sink that gets installed. Without this, those early
//! events fall through to ad-hoc `eprintln!` fallbacks and never reach
//! the TUI transcript (issue #391).

use std::sync::Arc;
use std::sync::Mutex;

use crate::event::{AgentEvent, AgentEventSink, AgentSource};

/// Cap on the pre-install buffer. Large enough to capture a handful of
/// startup warnings (one per misconfigured MCP server, plus a few
/// hook/config notices), small enough that we don't grow unboundedly
/// if a sink never gets installed (e.g. a non-interactive subcommand
/// that never enters the chat path).
const PENDING_EVENTS_CAP: usize = 64;

struct SinkState {
    sink: Option<Arc<dyn AgentEventSink>>,
    pending: Vec<(AgentEvent, Option<AgentSource>)>,
}

impl SinkState {
    const fn new() -> Self {
        Self {
            sink: None,
            pending: Vec::new(),
        }
    }
}

static AGENT_EVENT_SINK: Mutex<SinkState> = Mutex::new(SinkState::new());

pub fn install_agent_event_sink(sink: Arc<dyn AgentEventSink>) {
    // Take the buffered events out under the lock, install the new
    // sink, then drop the lock before replaying — replaying while
    // holding the lock would deadlock any sink whose `emit` reaches
    // back into the registry (none today, but cheap insurance).
    let pending = {
        let mut guard = AGENT_EVENT_SINK
            .lock()
            .expect("AGENT_EVENT_SINK mutex poisoned");
        guard.sink = Some(sink.clone());
        std::mem::take(&mut guard.pending)
    };
    for (event, source) in pending {
        sink.emit(event, source);
    }
}

pub fn has_agent_event_sink() -> bool {
    let guard = AGENT_EVENT_SINK
        .lock()
        .expect("AGENT_EVENT_SINK mutex poisoned");
    guard.sink.is_some()
}

pub fn emit_agent_event(event: AgentEvent) -> bool {
    emit_agent_event_with_source(event, None)
}

pub fn emit_agent_event_with_source(event: AgentEvent, source: Option<AgentSource>) -> bool {
    let sink = {
        let mut guard = AGENT_EVENT_SINK
            .lock()
            .expect("AGENT_EVENT_SINK mutex poisoned");
        match guard.sink.as_ref().cloned() {
            Some(sink) => sink,
            None => {
                if guard.pending.len() == PENDING_EVENTS_CAP {
                    guard.pending.remove(0);
                }
                guard.pending.push((event, source));
                return true;
            }
        }
    };
    sink.emit(event, source);
    true
}

/// Clear the installed sink and drop any buffered pending events.
/// Intended for test use — both harnx-core's own `#[cfg(test)]` tests
/// and harnx's cross-crate tests call this between cases to prevent
/// sink leakage.
pub fn clear_agent_event_sink() {
    let mut guard = AGENT_EVENT_SINK
        .lock()
        .expect("AGENT_EVENT_SINK mutex poisoned");
    guard.sink = None;
    guard.pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, NoticeEvent};
    use std::sync::Mutex;

    /// `AGENT_EVENT_SINK` is process-global state, but cargo runs tests in
    /// the same process in parallel. Without serialization, two tests
    /// racing on `clear_agent_event_sink` / `install_agent_event_sink` /
    /// `emit_agent_event` see each other's sinks and events. Acquire this
    /// guard at the top of every test that touches the global sink.
    static SINK_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Ignore `PoisonError` so a panic in one test doesn't cascade-fail
    /// every other sink test in this module.
    fn lock_sink_tests() -> std::sync::MutexGuard<'static, ()> {
        SINK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct CollectingSink {
        events: Mutex<Vec<AgentEvent>>,
    }

    impl CollectingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
            })
        }
    }

    impl AgentEventSink for CollectingSink {
        fn emit(&self, event: AgentEvent, _source: Option<AgentSource>) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[test]
    fn install_and_emit_cycle() {
        let _guard = lock_sink_tests();
        clear_agent_event_sink();
        // No sink installed yet — the event is buffered (delivered=true
        // because the caller no longer needs to fall back to ad-hoc
        // stderr output).
        let delivered = emit_agent_event(AgentEvent::Notice(NoticeEvent::Info("hi".into())));
        assert!(delivered);
        assert!(!has_agent_event_sink());

        let sink = CollectingSink::new();
        install_agent_event_sink(sink.clone());
        assert!(has_agent_event_sink());
        let delivered = emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning("whoa".into())));
        assert!(delivered);
        let events = sink.events.lock().unwrap();
        // Both the pre-install "hi" (replayed) and the post-install
        // "whoa" reach the sink in order.
        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::Notice(NoticeEvent::Info(msg)) => assert_eq!(msg, "hi"),
            other => panic!("unexpected first event: {other:?}"),
        }
        match &events[1] {
            AgentEvent::Notice(NoticeEvent::Warning(msg)) => assert_eq!(msg, "whoa"),
            other => panic!("unexpected second event: {other:?}"),
        }
        drop(events);
        clear_agent_event_sink();
    }

    #[test]
    fn pending_events_replayed_to_first_installed_sink() {
        use crate::event::AgentSource;
        let _guard = lock_sink_tests();
        clear_agent_event_sink();

        emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning("first".into())));
        emit_agent_event_with_source(
            AgentEvent::Notice(NoticeEvent::Warning("second".into())),
            Some(AgentSource {
                agent: "argus".into(),
                session_id: None,
            }),
        );

        let sink = Arc::new(SourceRecordingSink::default());
        install_agent_event_sink(sink.clone());

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        match &events[0].0 {
            AgentEvent::Notice(NoticeEvent::Warning(msg)) => assert_eq!(msg, "first"),
            other => panic!("unexpected first event: {other:?}"),
        }
        assert!(events[0].1.is_none());
        match &events[1].0 {
            AgentEvent::Notice(NoticeEvent::Warning(msg)) => assert_eq!(msg, "second"),
            other => panic!("unexpected second event: {other:?}"),
        }
        assert_eq!(events[1].1.as_ref().unwrap().agent, "argus");
        drop(events);
        clear_agent_event_sink();
    }

    #[test]
    fn pending_buffer_is_capped() {
        let _guard = lock_sink_tests();
        clear_agent_event_sink();

        for i in 0..(PENDING_EVENTS_CAP + 5) {
            emit_agent_event(AgentEvent::Notice(NoticeEvent::Info(format!("e{i}"))));
        }

        let sink = CollectingSink::new();
        install_agent_event_sink(sink.clone());

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), PENDING_EVENTS_CAP);
        // Oldest events are dropped first — first survivor is e5.
        match &events[0] {
            AgentEvent::Notice(NoticeEvent::Info(msg)) => assert_eq!(msg, "e5"),
            other => panic!("unexpected first event: {other:?}"),
        }
        match events.last().unwrap() {
            AgentEvent::Notice(NoticeEvent::Info(msg)) => {
                assert_eq!(msg, &format!("e{}", PENDING_EVENTS_CAP + 4))
            }
            other => panic!("unexpected last event: {other:?}"),
        }
        drop(events);
        clear_agent_event_sink();
    }

    #[derive(Default)]
    struct SourceRecordingSink {
        events: Mutex<Vec<(AgentEvent, Option<AgentSource>)>>,
    }

    impl AgentEventSink for SourceRecordingSink {
        fn emit(&self, event: AgentEvent, source: Option<AgentSource>) {
            self.events.lock().unwrap().push((event, source));
        }
    }

    #[test]
    fn emit_with_source_preserves_source() {
        use crate::event::AgentSource;
        let _guard = lock_sink_tests();
        clear_agent_event_sink();
        let sink = Arc::new(SourceRecordingSink::default());
        install_agent_event_sink(sink.clone());

        emit_agent_event(AgentEvent::Notice(NoticeEvent::Info("no-source".into())));
        emit_agent_event_with_source(
            AgentEvent::Notice(NoticeEvent::Info("with-source".into())),
            Some(AgentSource {
                agent: "argus".into(),
                session_id: Some("s1".into()),
            }),
        );

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].1.is_none());
        let (_, s1) = &events[1];
        let source = s1.as_ref().expect("second event carries source");
        assert_eq!(source.agent, "argus");
        assert_eq!(source.session_id.as_deref(), Some("s1"));
        drop(events);
        clear_agent_event_sink();
    }
}
