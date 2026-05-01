use harnx_core::event::{AgentSource, PlanEntry};
use harnx_hooks::{AsyncHookManager, PersistentHookManager};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::utils::AbortSignal;

use ratatui_textarea::TextArea;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

pub(super) const MIN_INPUT_HEIGHT: u16 = 3;
pub(super) const MAX_INPUT_HEIGHT: u16 = 8;
pub(super) const TICK_RATE: Duration = Duration::from_millis(80);
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Tui {
    pub(super) config: GlobalConfig,
    /// Tui-level abort signal used for Ctrl-D quitting and dot-command
    /// interruption. Each running prompt task gets its OWN abort signal
    /// (see `current_prompt_abort` below) so that resetting the Tui-level
    /// signal on a new submission can never un-abort an old prompt task.
    pub(super) abort_signal: AbortSignal,
    pub(super) async_manager: Arc<Mutex<AsyncHookManager>>,
    pub(super) persistent_manager: Arc<Mutex<PersistentHookManager>>,
    pub(super) pending_async_context: Arc<Mutex<Option<String>>>,
    /// Shared state so the prompt task can consume a pending message mid-tool-loop.
    pub(super) shared_pending_message: Arc<Mutex<Option<PendingMessage>>>,
    /// Per-task abort signal for the currently running (or most recently
    /// started) prompt task. Ctrl+C signals this; `start_prompt` consults
    /// it to abort an in-flight task before spawning a new one.
    pub(super) current_prompt_abort: Option<AbortSignal>,
    /// JoinHandle for the currently running (or most recently started)
    /// prompt task. `start_prompt` awaits/aborts this before spawning a
    /// new task — guaranteeing one prompt task at a time.
    pub(super) current_prompt_handle: Option<JoinHandle<()>>,
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
    pub(super) last_ui_output_source: Option<AgentSource>,
    pub(super) last_usage_source: Option<AgentSource>,
    pub(super) last_usage_transcript_idx: Option<usize>,
    pub(super) pending_thought_source: Option<AgentSource>,
    pub(super) pending_thought_text: String,
    pub(super) pending_message: Option<PendingMessage>,
    pub(super) completions: Vec<(String, Option<String>)>,
    pub(super) completion_index: usize,
    pub(super) completion_prefix: String,
    pub(super) completion_suffix: String,
    pub(super) history: Vec<String>,
    pub(super) history_index: Option<usize>,
    pub(super) history_draft: String,
    pub(super) history_preview: bool,
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

/// Body of a `ToolCall` transcript item. Distinguishes raw YAML (rendered
/// plainly) from rendered MiniJinja template text (rendered with inline
/// markdown styling). Mutually exclusive — a tool call has exactly one or
/// no body, never both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToolCallBody {
    /// YAML rendering of the raw tool-call arguments. Displayed verbatim.
    Yaml(String),
    /// Rendered MCP `call_template` output. Each line is treated as inline
    /// markdown (`**bold**`, `*italic*`, `` `code` ``).
    Markdown(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptItem {
    SourceHeading(AgentSource),
    SystemText(String),
    UserText {
        text: String,
        seq: Option<usize>,
    },
    AssistantText {
        text: String,
        seq: Option<usize>,
    },
    ErrorText(String),
    ThoughtText(String),
    /// Tool result body — the full multi-line text extracted from the
    /// MCP `CallToolResult`. Rendered through `markdown_lines` (with a
    /// dim base style) so block-level markdown like fenced diffs and
    /// inline emphasis from a `result_template` both display correctly.
    ToolResultMarkdown(String),
    StatusLine(String),
    Plan(Vec<PlanEntry>),
    UsageLine(String),
    ToolCall {
        tool_name: String,
        body: Option<ToolCallBody>,
        seq: Option<usize>,
    },
    AttachmentHeader(String),
    AttachmentItem(String),
    AttachmentPreviewLine(String),
    MutationNotice(String),
}

pub(crate) enum TuiEvent {
    Agent(
        harnx_core::event::AgentEvent,
        Option<harnx_core::event::AgentSource>,
    ),
    /// Intermediate tool round completed; the prompt loop continues.
    ToolRoundComplete,
    /// The prompt task consumed the pending message during a tool round.
    PendingMessageConsumed(PendingMessage),
}
