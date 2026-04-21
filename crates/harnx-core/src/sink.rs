//! Process-level `AgentEventSink` registry. Any code that generates
//! user-visible events can call `emit_agent_event` and have the active
//! sink (TUI, CLI, or none) render it. Lives in harnx-core so both
//! harnx and harnx-engine can reach it without cycles.
//!
//! Mirrors the production/test dual-storage pattern used elsewhere:
//! `OnceLock` in production for fast idempotent install; `Mutex` in
//! tests so installers can overwrite each other between test cases.

use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(not(test))]
use std::sync::OnceLock;

use crate::event::{AgentEvent, AgentEventSink};

#[cfg(test)]
static AGENT_EVENT_SINK: Mutex<Option<Arc<dyn AgentEventSink>>> = Mutex::new(None);

#[cfg(not(test))]
static AGENT_EVENT_SINK: OnceLock<Arc<dyn AgentEventSink>> = OnceLock::new();

#[cfg(not(test))]
pub fn install_agent_event_sink(sink: Arc<dyn AgentEventSink>) {
    let _ = AGENT_EVENT_SINK.set(sink);
}

#[cfg(test)]
pub fn install_agent_event_sink(sink: Arc<dyn AgentEventSink>) {
    let mut guard = AGENT_EVENT_SINK
        .lock()
        .expect("AGENT_EVENT_SINK mutex poisoned");
    *guard = Some(sink);
}

#[cfg(not(test))]
pub fn has_agent_event_sink() -> bool {
    AGENT_EVENT_SINK.get().is_some()
}

#[cfg(test)]
pub fn has_agent_event_sink() -> bool {
    let guard = AGENT_EVENT_SINK
        .lock()
        .expect("AGENT_EVENT_SINK mutex poisoned");
    guard.is_some()
}

#[cfg(not(test))]
pub fn emit_agent_event(event: AgentEvent) -> bool {
    match AGENT_EVENT_SINK.get() {
        Some(sink) => {
            sink.emit(event);
            true
        }
        None => false,
    }
}

#[cfg(test)]
pub fn emit_agent_event(event: AgentEvent) -> bool {
    let guard = AGENT_EVENT_SINK
        .lock()
        .expect("AGENT_EVENT_SINK mutex poisoned");
    match guard.as_ref() {
        Some(sink) => {
            sink.emit(event);
            true
        }
        None => false,
    }
}

#[cfg(test)]
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
        fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[test]
    fn install_and_emit_cycle() {
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
}
