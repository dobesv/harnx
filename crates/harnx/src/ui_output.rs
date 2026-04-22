use agent_client_protocol as acp;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(not(test))]
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;

use crate::utils::warning_text;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiOutputSource {
    pub agent: String,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum UiOutputEventKind {
    // Scheduled for deletion in Plan 38 Task 5. No production constructor
    // remains after Task 3 (the TuiAgentEventSink no longer translates
    // AgentEvent::Tool(Completed) into this variant); tests still
    // construct it via ui_output_to_agent_event and event_fallback_text.
    #[allow(dead_code)]
    ToolResultText {
        text: String,
    },
    MessageChunk {
        text: String,
        raw: Option<Box<acp::ContentChunk>>,
    },
    LlmFinal {
        output: String,
        usage: crate::client::CompletionTokenUsage,
    },
    LlmError(String),
    ThoughtChunk {
        text: String,
        raw: Option<Box<acp::ContentChunk>>,
    },
    ToolCall {
        tool_name: String,
        input_yaml: Option<String>,
        raw: Option<Box<acp::ToolCall>>,
    },
    ToolCallUpdate {
        tool_call_id: Option<String>,
        title: Option<String>,
        status: Option<String>,
        raw: Option<Box<acp::ToolCallUpdate>>,
    },
    TranscriptText {
        text: String,
    },
    Plan {
        entries: Vec<UiOutputPlanEntry>,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        session_label: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiOutputPlanEntry {
    pub status: String,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct UiOutputEvent {
    pub kind: UiOutputEventKind,
    pub source: Option<UiOutputSource>,
}

pub fn pretty_yaml_block(value: &serde_json::Value) -> String {
    serde_yaml::to_string(value)
        .map(|s| s.trim_end().to_string())
        .unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
static UI_OUTPUT_SENDER: Mutex<Option<UnboundedSender<UiOutputEvent>>> = Mutex::new(None);

#[cfg(not(test))]
static UI_OUTPUT_SENDER: OnceLock<UnboundedSender<UiOutputEvent>> = OnceLock::new();

#[cfg(not(test))]
pub fn install_ui_output_sender(sender: UnboundedSender<UiOutputEvent>) {
    let _ = UI_OUTPUT_SENDER.set(sender);
}

#[cfg(test)]
pub fn install_ui_output_sender(sender: UnboundedSender<UiOutputEvent>) {
    let mut guard = UI_OUTPUT_SENDER
        .lock()
        .expect("UI_OUTPUT_SENDER mutex poisoned");
    *guard = Some(sender);
}

#[cfg(not(test))]
pub fn has_ui_output_sink() -> bool {
    UI_OUTPUT_SENDER.get().is_some()
}

#[cfg(test)]
pub fn has_ui_output_sink() -> bool {
    let guard = UI_OUTPUT_SENDER
        .lock()
        .expect("UI_OUTPUT_SENDER mutex poisoned");
    guard.is_some()
}

// Scheduled for deletion in Plan 38 Task 5. After Task 3, the
// TuiAgentEventSink no longer calls this helper — production
// emitters have moved to the AgentEvent sink. Legacy harnx code
// (e.g. CLI install_cli_ui_output_sink, acp/client.rs
// forward_display_chunk) still invokes it, but not from this crate's
// lib build of main.rs — hence the lint. Remove along with
// install_ui_output_sender in Task 5.
#[cfg(not(test))]
#[allow(dead_code)]
pub fn emit_ui_output_event(event: UiOutputEvent) -> bool {
    match UI_OUTPUT_SENDER.get() {
        Some(sender) => sender.send(event).is_ok(),
        None => false,
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub fn emit_ui_output_event(event: UiOutputEvent) -> bool {
    let guard = UI_OUTPUT_SENDER
        .lock()
        .expect("UI_OUTPUT_SENDER mutex poisoned");
    match guard.as_ref() {
        Some(sender) => sender.send(event).is_ok(),
        None => false,
    }
}

/// Install a CLI-mode UI output sink that renders events to stderr.
///
/// This should be called once for `Cmd` working mode so that retry/fallback
/// warnings and other transcript events are printed to stderr instead of
/// being silently dropped.
pub fn install_cli_ui_output_sink() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    install_ui_output_sender(tx);
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event.kind {
                UiOutputEventKind::TranscriptText { text } => {
                    eprintln!("{}", warning_text(&text));
                }
                UiOutputEventKind::LlmError(text) => {
                    eprintln!("{}", warning_text(&format!("LLM error: {text}")));
                }
                // Other event kinds are handled inline by the CLI callers
                // (streaming output, tool calls, etc.) and don't need to be
                // duplicated here.
                _ => {}
            }
        }
    });
}

#[allow(dead_code)]
pub fn clear_ui_output_sender() {
    #[cfg(test)]
    {
        let mut guard = UI_OUTPUT_SENDER
            .lock()
            .expect("UI_OUTPUT_SENDER mutex poisoned");
        *guard = None;
    }
    #[cfg(not(test))]
    {
        // OnceLock cannot be cleared; this is a no-op in production.
        // In production, each process has exactly one sender for its lifetime.
    }
}
