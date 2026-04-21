//! `CliAgentEventSink` renders `AgentEvent`s to stderr. Used by the
//! non-interactive CLI mode (and by integration tests) when the engine
//! emits events. Common variants get dedicated formatting; less-common
//! variants fall through to a `{:?}` Debug line until they get a
//! dedicated renderer in a later plan.

use std::sync::{Arc, Mutex};

use harnx_core::event::{
    AgentEvent, AgentEventSink, ModelEvent, NoticeEvent, ToolEvent, TurnEvent,
};

use crate::render::{MarkdownRender, RenderOptions};
use crate::utils::{dimmed_text, warning_text, Spinner};

/// Stderr-bound sink for the non-interactive CLI. Thread-safe — interior
/// state is held behind an `Arc<Mutex<CliSinkState>>` so multiple clones
/// of the sink share the same spinner/render buffer.
#[derive(Clone)]
pub struct CliAgentEventSink {
    state: Arc<Mutex<CliSinkState>>,
}

// Fields are populated structurally here but only consumed by the
// chunk-rendering handlers added in Plan 33 Task 2. The `dead_code`
// allow is transient — removed in the Task 2 commit once the
// handlers reference every field.
#[allow(dead_code)]
struct CliSinkState {
    spinner: Option<Spinner>,
    render: Option<MarkdownRender>,
    buffer: String,
    buffer_rows: u16,
    columns: u16,
    raw_mode_active: bool,
    highlight: bool,
    render_options: RenderOptions,
}

impl CliAgentEventSink {
    pub fn new(highlight: bool, render_options: RenderOptions) -> Self {
        Self {
            state: Arc::new(Mutex::new(CliSinkState {
                spinner: None,
                render: None,
                buffer: String::new(),
                buffer_rows: 1,
                columns: 0,
                raw_mode_active: false,
                highlight,
                render_options,
            })),
        }
    }
}

impl AgentEventSink for CliAgentEventSink {
    fn emit(&self, event: AgentEvent) {
        let state = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
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
            AgentEvent::Model(ModelEvent::MessageChunk { .. }) => {
                // CLI streaming mode uses render_stream to write chunks to stdout.
                // Emitting here (stderr via eprint) would duplicate the display.
                // Plan 33 will retire render_stream and flip this arm to a stdout
                // markdown/raw renderer.
            }
            AgentEvent::Model(ModelEvent::ThoughtChunk { .. }) => {
                // Same rationale — render_stream handles thought display via the
                // channel's prefix/suffix <think>...</think> bracketing.
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
            AgentEvent::Tool(ToolEvent::Started { name, .. }) => {
                eprintln!("{}", dimmed_text(&format!("[tool] {name}")));
            }
            AgentEvent::Tool(ToolEvent::Failed { error, .. }) => {
                eprintln!("{}", warning_text(&format!("tool error: {error}")));
            }
            // Silent for Progress / Update / Completed: CLI doesn't stream
            // per-chunk tool updates; Completed's output is usually internal
            // and shouldn't clutter stderr. Users see tool effects via the
            // subsequent LLM response.
            AgentEvent::Tool(_) => {}
            // Every other variant — Session, Status, Plan — still gets
            // captured so nothing silently disappears. These receive dedicated
            // renderers in a future plan.
            other => eprintln!("{}", dimmed_text(&format!("[event] {other:?}"))),
        }
        let _ = &state; // state is unused in Task 1; Task 2 wires chunk rendering.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harnx_core::event::ContentBlock;

    // The sink writes to stderr, which is hard to capture in a unit test
    // without subprocess machinery. We verify here only that `emit` doesn't
    // panic for a representative sample of event variants — the behavioral
    // verification (events arrive in the right order) lives in the
    // integration test at `tests/engine_smoke.rs`.

    #[test]
    fn emit_handles_each_top_level_variant_without_panic() {
        let sink = CliAgentEventSink::new(false, RenderOptions::default());

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
