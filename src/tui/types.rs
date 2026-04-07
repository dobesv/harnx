use crate::client::CompletionTokenUsage;
use crate::config::GlobalConfig;
use crate::hooks::{AsyncHookManager, PersistentHookManager};
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
    #[cfg(test)]
    #[allow(private_interfaces)]
    pub(crate) app: App,
    #[cfg(not(test))]
    #[allow(dead_code)]
    pub(super) app: App,
    #[cfg(test)]
    pub(crate) event_tx: mpsc::UnboundedSender<TuiEvent>,
    #[cfg(not(test))]
    pub(super) event_tx: mpsc::UnboundedSender<TuiEvent>,
    #[cfg(test)]
    pub(crate) event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    #[cfg(not(test))]
    pub(super) event_rx: mpsc::UnboundedReceiver<TuiEvent>,
}

#[derive(Clone, Debug)]
pub(super) struct Attachment {
    pub(super) path: PathBuf,
    pub(super) display_name: String,
    /// If true, the file at `path` is a temp file created by paste and should
    /// be deleted when the attachment is sent or detached.
    pub(super) temp: bool,
}

impl Attachment {
    /// Remove the backing file if this is a temp attachment.
    pub(super) fn cleanup(&self) {
        if self.temp {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone)]
pub(super) struct PendingMessage {
    pub(super) text: String,
    pub(super) attachments: Vec<Attachment>,
}

pub(super) struct App {
    pub(super) transcript: Vec<TranscriptEntry>,
    pub(super) input: TextArea<'static>,
    pub(super) spinner_index: usize,
    pub(super) should_quit: bool,
    pub(super) llm_busy: bool,
    pub(super) transcript_scroll: u16,
    pub(super) max_scroll: u16,
    pub(super) streaming_assistant_idx: Option<usize>,
    pub(super) pending_message: Option<PendingMessage>,
    pub(super) completions: Vec<(String, Option<String>)>,
    pub(super) completion_index: usize,
    pub(super) completion_prefix: String,
    pub(super) completion_suffix: String,
    pub(super) history: Vec<String>,
    pub(super) history_index: Option<usize>,
    pub(super) history_draft: String,
    pub(super) attachments: Vec<Attachment>,
    pub(super) last_known_input_width: u16,
}

#[derive(Clone)]
#[cfg(test)]
pub(crate) enum TranscriptEntry {
    System(String),
    User(String),
    Assistant(String),
    Error(String),
}

#[cfg(not(test))]
pub(super) enum TranscriptEntry {
    System(String),
    User(String),
    Assistant(String),
    Error(String),
}

#[cfg(test)]
pub(crate) enum TuiEvent {
    UiOutput(String),
    Chunk(String),
    /// Intermediate tool round completed; the prompt loop continues.
    ToolRoundComplete {
        tool_count: usize,
    },
    /// Final completion — no more turns.
    Finished {
        output: String,
        usage: CompletionTokenUsage,
    },
    Errored(String),
}

#[cfg(not(test))]
pub(super) enum TuiEvent {
    UiOutput(String),
    Chunk(String),
    /// Intermediate tool round completed; the prompt loop continues.
    ToolRoundComplete {
        tool_count: usize,
    },
    /// Final completion — no more turns.
    Finished {
        output: String,
        usage: CompletionTokenUsage,
    },
    Errored(String),
}
