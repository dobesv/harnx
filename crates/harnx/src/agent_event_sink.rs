//! Process-level `AgentEventSink` registry. Any harnx code that
//! generates user-visible events can call `emit_agent_event` and have
//! the active sink (TUI, CLI, or none) render it. Mirrors the
//! `UI_OUTPUT_SENDER` pattern in `ui_output.rs` — single sink per
//! process, installed at front-end startup.
//!
//! The long-term design (see
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md`)
//! threads `SessionCtx` through the engine for per-turn sink scoping.
//! This global exists as a bridge: legacy harnx emitters that haven't
//! migrated to `SessionCtx` plumbing can still reach the unified
//! event channel.

use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(not(test))]
use std::sync::OnceLock;

use harnx_core::event::{AgentEvent, AgentEventSink};

use crate::cli_event_sink::CliAgentEventSink;
use crate::ui_output::{emit_ui_output_event, UiOutputEvent, UiOutputEventKind};

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

/// Emit `event` to the installed sink. Returns `true` if delivered,
/// `false` if no sink is installed. Callers can use the return value
/// to decide whether to fall back to direct stderr printing.
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

/// Install the stderr-backed `CliAgentEventSink`. Used by the CLI
/// (`Cmd`) working mode at process startup.
pub fn install_cli_agent_event_sink() {
    install_agent_event_sink(Arc::new(CliAgentEventSink));
}

/// Sink used by the interactive TUI mode. Translates `AgentEvent`
/// into the legacy `UiOutputEvent` representation and forwards to
/// the existing TUI consumer via `emit_ui_output_event`.
///
/// This bridge exists because the TUI's event-loop still consumes
/// `UiOutputEvent` today. When the TUI migrates to consume `AgentEvent`
/// directly, this struct can be replaced with a thinner renderer.
pub struct TuiAgentEventSink;

impl AgentEventSink for TuiAgentEventSink {
    fn emit(&self, event: AgentEvent) {
        use harnx_core::event::NoticeEvent;
        // Only Notice variants need translation today — retry warnings
        // and similar operator-facing messages. Other variants (Turn,
        // Model, Tool, Session, Status, Plan) are still emitted via
        // `emit_ui_output_event` directly by the harnx code that
        // creates them. When those call sites migrate to AgentEvent,
        // add translation branches here.
        if let AgentEvent::Notice(notice) = event {
            let text = match notice {
                NoticeEvent::Info(msg) => msg,
                NoticeEvent::Warning(msg) => format!("⚠ {msg}"),
                NoticeEvent::Error(msg) => format!("error: {msg}"),
            };
            let ui_event = UiOutputEvent {
                kind: UiOutputEventKind::TranscriptText { text },
                source: None,
            };
            emit_ui_output_event(ui_event);
        }
    }
}

/// Install the `TuiAgentEventSink`. Called by TUI-mode startup
/// alongside the existing `install_ui_output_sender` for the legacy
/// `UiOutputEvent` channel.
pub fn install_tui_agent_event_sink() {
    install_agent_event_sink(Arc::new(TuiAgentEventSink));
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
    use harnx_core::event::{AgentEvent, NoticeEvent};
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

    // Merged into a single `#[test]` fn to avoid races on the global
    // `AGENT_EVENT_SINK` — nextest runs tests in parallel by default
    // and both phases here mutate the global registry.
    #[test]
    fn install_and_emit_cycle() {
        clear_agent_event_sink();

        // Phase 1: no sink installed — emit returns false.
        let delivered = emit_agent_event(AgentEvent::Notice(NoticeEvent::Info("hi".into())));
        assert!(!delivered);
        assert!(!has_agent_event_sink());

        // Phase 2: install a collecting sink, emit, verify delivery.
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
