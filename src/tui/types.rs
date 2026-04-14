use crate::config::GlobalConfig;
use crate::hooks::{AsyncHookManager, PersistentHookManager};
use crate::ui_output::{UiOutputEvent, UiOutputSource};
use crate::utils::AbortSignal;

use ratatui_textarea::TextArea;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

pub(super) const MIN_INPUT_HEIGHT: u16 = 3;
pub(super) const MAX_INPUT_HEIGHT: u16 = 8;
pub(super) const TICK_RATE: Duration = Duration::from_millis(80);
pub(super) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Tui {
    pub(super) config: GlobalConfig,
    pub(super) abort_signal: AbortSignal,
    pub(super) async_manager: Arc<Mutex<AsyncHookManager>>,
    pub(super) persistent_manager: Arc<Mutex<PersistentHookManager>>,
    pub(super) pending_async_context: Arc<Mutex<Option<String>>>,
    /// Shared state so the prompt task can consume a pending message mid-tool-loop.
    pub(super) shared_pending_message: Arc<Mutex<Option<PendingMessage>>>,
    #[allow(private_interfaces)]
    pub(crate) app: App,
    pub(crate) event_tx: mpsc::UnboundedSender<TuiEvent>,
    pub(crate) event_rx: mpsc::UnboundedReceiver<TuiEvent>,
}

#[derive(Clone, Debug)]
pub(super) struct Attachment {
    pub(super) path: PathBuf,
    pub(super) display_name: String,
}

#[derive(Clone)]
pub(crate) struct PendingMessage {
    pub(super) text: String,
    pub(super) attachments: Vec<Attachment>,
    pub(super) attachment_dir: Option<PathBuf>,
    pub(super) paste_count: usize,
}

pub(super) struct App {
    pub(super) transcript: Vec<TranscriptItem>,
    pub(super) input: TextArea<'static>,
    pub(super) spinner_index: usize,
    pub(super) should_quit: bool,
    pub(super) llm_busy: bool,
    pub(super) scroll_state: ratatui_widget_scrolling::ScrollState,
    pub(super) streaming_assistant_idx: Option<usize>,
    pub(super) last_ui_output_source: Option<UiOutputSource>,
    pub(super) last_usage_source: Option<UiOutputSource>,
    pub(super) last_usage_transcript_idx: Option<usize>,
    pub(super) pending_thought_source: Option<UiOutputSource>,
    pub(super) pending_thought_text: String,
    pub(super) pending_message: Option<PendingMessage>,
    pub(super) completions: Vec<(String, Option<String>)>,
    pub(super) completion_index: usize,
    pub(super) completion_prefix: String,
    pub(super) completion_suffix: String,
    pub(super) history: Vec<String>,
    pub(super) history_index: Option<usize>,
    pub(super) history_draft: String,
    pub(super) attachments: Vec<Attachment>,
    /// Temp directory holding copies of all current attachments. Created on
    /// first attach, removed recursively on submit or full detach.
    pub(super) attachment_dir: Option<PathBuf>,
    pub(super) paste_count: usize,
    pub(super) last_known_input_width: u16,
}

/// Create a unique temporary attachment directory in the system temp area.
pub(super) fn create_attachment_dir() -> std::io::Result<PathBuf> {
    for _ in 0..16 {
        let dir = std::env::temp_dir().join(format!("harnx-attach-{}", uuid::Uuid::new_v4()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to create unique attachment directory",
    ))
}

/// Remove the attachment directory and all its contents.
pub(super) fn cleanup_attachment_dir(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptItem {
    SourceHeading(UiOutputSource),
    SystemText(String),
    UserText(String),
    AssistantText(String),
    ErrorText(String),
    ThoughtText(String),
    ToolResultText(String),
    StatusLine(String),
    Plan(Vec<crate::ui_output::UiOutputPlanEntry>),
    UsageLine(String),
    ToolCall {
        tool_name: String,
        input_yaml: Option<String>,
    },
    AttachmentHeader(String),
    AttachmentItem(String),
    AttachmentPreviewLine(String),
}

pub(crate) enum TuiEvent {
    UiOutput(UiOutputEvent),
    /// Intermediate tool round completed; the prompt loop continues.
    ToolRoundComplete,
    /// The prompt task consumed the pending message during a tool round.
    PendingMessageConsumed(PendingMessage),
}
