#[cfg(test)]
use std::sync::Mutex;
#[cfg(not(test))]
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiOutputSource {
    pub agent: String,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum UiOutputEventKind {
    ToolResultText {
        text: String,
    },
    LlmText(String),
    LlmFinal {
        output: String,
        usage: crate::client::CompletionTokenUsage,
    },
    LlmError(String),
    AcpThought {
        text: String,
    },
    StatusLine {
        text: String,
    },
    TranscriptText {
        text: String,
    },
    StructuredBlock {
        title: String,
        body: Option<String>,
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
    McpToolInvocation {
        tool_name: String,
        input_yaml: Option<String>,
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

#[cfg(not(test))]
pub fn emit_ui_output_event(event: UiOutputEvent) -> bool {
    match UI_OUTPUT_SENDER.get() {
        Some(sender) => sender.send(event).is_ok(),
        None => false,
    }
}

#[cfg(test)]
pub fn emit_ui_output_event(event: UiOutputEvent) -> bool {
    let guard = UI_OUTPUT_SENDER
        .lock()
        .expect("UI_OUTPUT_SENDER mutex poisoned");
    match guard.as_ref() {
        Some(sender) => sender.send(event).is_ok(),
        None => false,
    }
}

#[cfg(test)]
pub fn clear_ui_output_sender() {
    let mut guard = UI_OUTPUT_SENDER
        .lock()
        .expect("UI_OUTPUT_SENDER mutex poisoned");
    *guard = None;
}
