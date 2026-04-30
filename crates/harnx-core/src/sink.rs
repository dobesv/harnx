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

use std::sync::Arc;
use std::sync::Mutex;

use crate::event::{AgentEvent, AgentEventSink, AgentSource};

static AGENT_EVENT_SINK: Mutex<Option<Arc<dyn AgentEventSink>>> = Mutex::new(None);

pub fn install_agent_event_sink(sink: Arc<dyn AgentEventSink>) {
    let mut guard = AGENT_EVENT_SINK
        .lock()
        .expect("AGENT_EVENT_SINK mutex poisoned");
    *guard = Some(sink);
}

pub fn has_agent_event_sink() -> bool {
    let guard = AGENT_EVENT_SINK
        .lock()
        .expect("AGENT_EVENT_SINK mutex poisoned");
    guard.is_some()
}

pub fn emit_agent_event(event: AgentEvent) -> bool {
    emit_agent_event_with_source(event, None)
}

pub fn emit_agent_event_with_source(event: AgentEvent, source: Option<AgentSource>) -> bool {
    let sink = {
        let guard = AGENT_EVENT_SINK
            .lock()
            .expect("AGENT_EVENT_SINK mutex poisoned");
        guard.as_ref().cloned()
    };
    match sink {
        Some(sink) => {
            sink.emit(event, source);
            true
        }
        None => false,
    }
}

/// Clear the installed sink. Intended for test use — both harnx-core's
/// own `#[cfg(test)]` tests and harnx's cross-crate tests call this
/// between cases to prevent sink leakage.
pub fn clear_agent_event_sink() {
    let mut guard = AGENT_EVENT_SINK
        .lock()
        .expect("AGENT_EVENT_SINK mutex poisoned");
    *guard = None;
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
        let delivered = emit_agent_event(AgentEvent::Notice(NoticeEvent::Info("hi".into())));
        assert!(!delivered);
        assert!(!has_agent_event_sink());

        let sink = CollectingSink::new();
        install_agent_event_sink(sink.clone());
        assert!(has_agent_event_sink());
        let delivered = emit_agent_event(AgentEvent::Notice(NoticeEvent::Warning("whoa".into())));
        assert!(delivered);
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Notice(NoticeEvent::Warning(msg)) => assert_eq!(msg, "whoa"),
            other => panic!("unexpected event: {other:?}"),
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
