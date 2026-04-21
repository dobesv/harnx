//! `CliAgentEventSink` renders `AgentEvent`s to stderr. Used by the
//! non-interactive CLI mode (and by integration tests) when the engine
//! emits events. Common variants get dedicated formatting; less-common
//! variants fall through to a `{:?}` Debug line until they get a
//! dedicated renderer in a later plan.

use harnx_core::event::{
    AgentEvent, AgentEventSink, ContentBlock, ModelEvent, NoticeEvent, TurnEvent,
};

use crate::utils::{dimmed_text, warning_text};

/// Stderr-bound sink for the non-interactive CLI. Thread-safe (interior
/// writes go through `std::io::stderr()`).
#[derive(Debug, Default, Clone, Copy)]
pub struct CliAgentEventSink;

impl AgentEventSink for CliAgentEventSink {
    fn emit(&self, event: AgentEvent) {
        match &event {
            AgentEvent::Turn(TurnEvent::Started) => {
                // Start-of-turn is silent on the CLI — the prompt echo already tells
                // the user something is happening.
            }
            AgentEvent::Turn(TurnEvent::Ended { outcome: _ }) => {
                // End-of-turn is also silent; the final message was already streamed.
            }
            AgentEvent::Turn(TurnEvent::RetryAttempt { attempt, reason }) => {
                eprintln!("{}", warning_text(&format!("retry #{attempt}: {reason}")));
            }
            AgentEvent::Turn(TurnEvent::ModelFallback { from, to }) => {
                eprintln!(
                    "{}",
                    warning_text(&format!("model fallback: {from} → {to}"))
                );
            }
            AgentEvent::Turn(TurnEvent::HandoffRequested { agent, .. }) => {
                eprintln!("{}", dimmed_text(&format!("handoff → {agent}")));
            }
            AgentEvent::Notice(NoticeEvent::Info(msg)) => {
                eprintln!("{msg}");
            }
            AgentEvent::Notice(NoticeEvent::Warning(msg)) => {
                eprintln!("{}", warning_text(msg));
            }
            AgentEvent::Notice(NoticeEvent::Error(msg)) => {
                eprintln!("{}", warning_text(&format!("error: {msg}")));
            }
            AgentEvent::Model(ModelEvent::MessageChunk { blocks }) => {
                for block in blocks {
                    if let ContentBlock::Text(text) = block {
                        eprint!("{text}");
                    }
                }
            }
            AgentEvent::Model(ModelEvent::Final { output, .. }) => {
                // If streaming produced no chunks, print the full output at once.
                if !output.is_empty() {
                    eprintln!("{output}");
                }
            }
            AgentEvent::Model(ModelEvent::Error(err)) => {
                eprintln!("{}", warning_text(&format!("LLM error: {err}")));
            }
            AgentEvent::Model(ModelEvent::Usage {
                input,
                output,
                cached,
                session_label: _,
            }) => {
                // Emit a compact one-liner on stderr when we have non-zero usage.
                if *input > 0 || *output > 0 || *cached > 0 {
                    let cached_suffix = if *cached > 0 {
                        format!(" (cached {cached})")
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "{}",
                        dimmed_text(&format!("[tokens] in={input} out={output}{cached_suffix}"))
                    );
                }
            }
            // Every other variant — Tool, Session, Status, Plan — still gets
            // captured so nothing silently disappears. These receive dedicated
            // renderers in a future plan.
            other => eprintln!("{}", dimmed_text(&format!("[event] {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The sink writes to stderr, which is hard to capture in a unit test
    // without subprocess machinery. We verify here only that `emit` doesn't
    // panic for a representative sample of event variants — the behavioral
    // verification (events arrive in the right order) lives in the
    // integration test at `tests/engine_smoke.rs`.

    #[test]
    fn emit_handles_each_top_level_variant_without_panic() {
        let sink = CliAgentEventSink;

        sink.emit(AgentEvent::Turn(TurnEvent::Started));
        sink.emit(AgentEvent::Turn(TurnEvent::Ended {
            outcome: Default::default(),
        }));
        sink.emit(AgentEvent::Notice(NoticeEvent::Info("info".into())));
        sink.emit(AgentEvent::Notice(NoticeEvent::Warning("warn".into())));
        sink.emit(AgentEvent::Notice(NoticeEvent::Error("err".into())));
        sink.emit(AgentEvent::Model(ModelEvent::MessageChunk {
            blocks: vec![ContentBlock::Text("hello".into())],
        }));
        sink.emit(AgentEvent::Model(ModelEvent::Final {
            output: "done".into(),
            usage: Default::default(),
        }));
        sink.emit(AgentEvent::Model(ModelEvent::Error("boom".into())));
    }
}
