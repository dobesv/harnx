use harnx_core::event::{AgentSource, PlanEntry, SubAgentProgress, SubAgentProgressStatus};
use harnx_runtime::config::GlobalConfig;
use harnx_runtime::config::SessionMeta;
use harnx_runtime::local_orchestrator::LocalWorkerSupervisor;
use harnx_runtime::utils::AbortSignal;

use crate::markdown_render::RenderedEntry;
use crate::tool_confirmation::ToolConfirmationReply;
use chrono::{DateTime, Utc};
use ratatui_textarea::TextArea;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use syntect::highlighting::Theme;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

pub(super) const MIN_INPUT_HEIGHT: u16 = 3;
pub(super) const MAX_INPUT_HEIGHT: u16 = 8;
pub(super) const TICK_RATE: Duration = Duration::from_millis(80);
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitWorkerState {
    Remote,
    LocalOwnedHere,
    LocalOwnedElsewhere,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitPhase {
    Prompting,
    Interrupting,
    RequestFailed,
}

pub(crate) type ExitCancelFuture = Pin<
    Box<dyn Future<Output = anyhow::Result<harnx_runtime::nats_session::InterruptOutcome>> + Send>,
>;
pub(crate) type ExitCancelFactory = Arc<
    dyn Fn(
            GlobalConfig,
            Arc<Mutex<Option<LocalWorkerSupervisor>>>,
            String,
            String,
        ) -> ExitCancelFuture
        + Send
        + Sync,
>;
/// The task running an [`ExitCancelFuture`]; see `Tui::pending_exit_cancel`.
pub(crate) type PendingExitCancel =
    JoinHandle<anyhow::Result<harnx_runtime::nats_session::InterruptOutcome>>;

pub struct Tui {
    pub(super) config: GlobalConfig,
    pub(super) code_theme: Option<Theme>,
    /// Set to `true` by the after-editor hook so the main loop can call
    /// `terminal.clear()` before the next draw, forcing a full repaint after
    /// the external editor exits and the TUI re-enters the alternate screen.
    pub(super) needs_full_redraw: Arc<std::sync::atomic::AtomicBool>,
    /// Tui-level abort signal used for Ctrl-D quitting and dot-command
    /// interruption. Each running prompt task gets its OWN abort signal
    /// (see `current_prompt_abort` below) so that resetting the Tui-level
    /// signal on a new submission can never un-abort an old prompt task.
    pub(super) abort_signal: AbortSignal,
    pub(super) pending_async_context: Arc<Mutex<Option<String>>>,
    /// Shared state so the prompt task can consume a pending message mid-tool-loop.
    pub(super) shared_pending_message: Arc<Mutex<Option<PendingMessage>>>,
    /// Lazily-created local broker/worker owner. Shared with prompt tasks and
    /// retained for the full TUI lifetime.
    pub(super) local_worker: Arc<Mutex<Option<LocalWorkerSupervisor>>>,
    /// Per-task abort signal for the currently running (or most recently
    /// started) prompt task. Ctrl+C signals this; `start_prompt` consults
    /// it to abort an in-flight task before spawning a new one.
    pub(super) current_prompt_abort: Option<AbortSignal>,
    pub(crate) live_events: harnx_runtime::nats_event_sink::LiveEventState,
    /// JoinHandle for the currently running (or most recently started)
    /// prompt task. `start_prompt` awaits/aborts this before spawning a
    /// new task — guaranteeing one prompt task at a time.
    pub(super) current_prompt_handle: Option<JoinHandle<()>>,
    /// The (session_id, cluster) of the remote agent currently running, if any.
    /// Set in `start_prompt` and cleared when the turn completes.
    pub(super) active_remote_session: Option<(String, String)>,
    /// Builds the durable cancel operation used by interrupt-and-exit.
    /// Spawned by `start_cancellation`; its handle is checked in
    /// `poll_pending_exit_cancel`.
    pub(crate) exit_cancel_factory: ExitCancelFactory,
    /// In-flight durable cancel, running as its own task. The event loop only
    /// checks the handle once per tick. The request awaits one JetStream round
    /// trip per session log entry and spends a wall-clock budget on the append,
    /// so driving it one wake-up per tick made every interrupt of a long
    /// session time out. Must complete before `should_quit`, so process exit
    /// (which drops `LocalWorkerSupervisor` and kills the worker) does not race
    /// the cancel request. Dropping the handle would detach the task, not stop
    /// it, so `run` aborts one still pending when the loop exits.
    pub(crate) pending_exit_cancel: Option<PendingExitCancel>,
    pub(crate) cancellation: Option<crate::cancellation::CancellationTray>,
    pub(crate) exit_after_cancel: bool,
    /// Deferred warning text emitted after terminal restoration.
    pub(crate) exit_interrupt_error: Option<String>,
    /// Confirmation route retained for the frontend's current session. Prompt
    /// and busy-enqueue paths share it so queued continuations keep a live modal.
    pub(super) tool_confirmation_route: SharedToolConfirmationRoute,
    /// Deterministic enqueue seam for confirmation orchestration tests.
    #[cfg(test)]
    pub(super) confirmation_enqueue_override:
        Option<crate::tool_confirmation::TestConfirmationEnqueueFn>,
    /// Sessions whose durable text append succeeded but whose worker activation
    /// must be retried without submitting the text as a second user message.
    pub(super) pending_remote_activations: HashSet<(String, String)>,
    /// Session whose shared NATS turn activity is currently being observed.
    pub(super) session_activity_target: Option<(String, String)>,
    /// Background subscription that lets this TUI react to turns submitted by
    /// another client attached to the same session.
    pub(super) session_activity_handle: Option<JoinHandle<()>>,
    /// Root session whose nested sub-agent monitors belong to. Changing the
    /// root aborts every child monitor and drops their retained views.
    pub(super) subagent_monitor_root: Option<(String, String)>,
    /// Independent live subscriptions for child sessions.
    pub(super) subagent_monitor_handles: HashMap<MonitoredSessionKey, JoinHandle<()>>,
    /// Set when a durable transcript rebuild may have introduced child rows.
    /// The next monitor sync consumes it instead of rescanning every frame.
    pub(super) subagent_rows_dirty: bool,
    #[allow(private_interfaces)]
    pub(crate) app: App,
    pub(crate) event_tx: mpsc::UnboundedSender<TuiEvent>,
    pub(crate) event_rx: mpsc::UnboundedReceiver<TuiEvent>,
}

pub(super) type SharedToolConfirmationRoute =
    Arc<parking_lot::Mutex<Option<ActiveToolConfirmationRoute>>>;

pub(super) struct ActiveToolConfirmationRoute {
    pub(super) target: (String, String),
    pub(super) route: ToolConfirmationRouteHandle,
}

#[derive(Clone)]
pub(super) enum ToolConfirmationRouteHandle {
    Nats(
        Arc<harnx_runtime::nats_tool_confirmation::ToolConfirmationRoute>,
        Arc<std::sync::atomic::AtomicBool>,
    ),
    #[cfg(test)]
    Test(Arc<std::sync::atomic::AtomicBool>),
}

impl ToolConfirmationRouteHandle {
    pub(super) fn is_closed(&self) -> bool {
        match self {
            Self::Nats(_, closed) => closed.load(std::sync::atomic::Ordering::Acquire),
            #[cfg(test)]
            Self::Test(closed) => closed.load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    pub(super) fn shutdown(&self) {
        match self {
            Self::Nats(route, closed) => {
                closed.store(true, std::sync::atomic::Ordering::Release);
                route.shutdown();
            }
            #[cfg(test)]
            Self::Test(shutdown) => shutdown.store(true, std::sync::atomic::Ordering::SeqCst),
        }
    }

    pub(super) fn nats(
        &self,
    ) -> Option<Arc<harnx_runtime::nats_tool_confirmation::ToolConfirmationRoute>> {
        match self {
            Self::Nats(route, _) => Some(Arc::clone(route)),
            #[cfg(test)]
            Self::Test(_) => None,
        }
    }
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
    /// True while the trailing `AssistantText` transcript item is an open
    /// streaming run that subsequent `MessageChunk`s should be appended to.
    /// Set false at every turn boundary (Final, Error, new prompt) so a
    /// finalized message — or the startup banner — is never appended to.
    /// Interleaving items (tool calls, notices, headings, …) end a run
    /// implicitly by becoming the trailing item themselves.
    pub(super) streaming_open: bool,
    /// Transcript row containing the latest streamed parent-agent text for
    /// this turn. Sub-agent rows must never become the replacement target for
    /// the parent agent's canonical `ModelEvent::Final` output.
    pub(super) main_streamed_text_idx: Option<usize>,
    pub(super) cache_valid_width: Option<u16>,
    pub(super) last_ui_output_source: Option<AgentSource>,
    pub(super) pending_thought_source: Option<AgentSource>,
    pub(super) pending_thought_text: String,
    pub(super) pending_tool_seq: Option<usize>,
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
    pub(super) show_sequence_numbers: bool,
    pub(super) show_timestamps: bool,
    /// Index of the cursor item in the transcript (None = input focused).
    /// Used by D2 for Up/Down navigation within transcript.
    #[allow(dead_code)]
    pub(super) transcript_focus: Option<usize>,
    /// Anchor index for shift-select range.
    /// Used by D2 for extending selection with Shift+Up/Down.
    #[allow(dead_code)]
    pub(super) transcript_selection_anchor: Option<usize>,
    /// Modal dialog state for destructive action confirmations.
    pub(super) modal: Option<ModalState>,
    /// Reply channel for an in-flight tool-use confirmation. Set alongside a
    /// `ModalState::ConfirmToolUse`; the confirmation handler waits on the
    /// receiver, and answering the modal sends the decision here.
    pub(super) pending_confirm_reply: Option<ToolConfirmationReply>,
    /// Identity of `pending_confirm_reply`. Remote handlers use it to dismiss
    /// only their own modal when its confirmation wait is cancelled.
    pub(super) pending_confirm_id: Option<u64>,
    /// Cached unread state of the current session. Updated on session change
    /// and when read-invalidation events arrive. Used to show indicator in
    /// input title and to gate mark-read calls (only emit when unread).
    ///
    /// Can be stale relative to durable KV because: (1) `run_loop_inner` drains
    /// `event_rx` after handling key input, so queued invalidations haven't been
    /// applied yet; (2) the session activity monitor stops during prompt execution,
    /// dropping non-durable read-invalidations. Terminal exit paths (idle Ctrl+D)
    /// should bypass this cache and mark read directly via `mark_current_session_read(true)`.
    pub(super) current_session_unread: bool,
    pub(super) detail_view_scroll: ratatui_widget_scrolling::ScrollState,
    pub(super) detail_view_open: bool,
    pub(super) detail_view_text: Option<String>,
    /// Passive child-session entry shown in the shared detail surface.
    /// Child transcripts are not editable, so this is kept separate from the
    /// root transcript selection and its mutation actions.
    pub(super) detail_view_entry: Option<TranscriptItem>,
    /// Title shown for a `detail_view_text` overlay (e.g. "Compacted session",
    /// "Agent Info", "Session Info"). None falls back to the generic "Detail".
    pub(super) detail_view_title: Option<String>,
    /// True when the user is browsing history in fullscreen mode.
    /// Distinct from detail_view_open, which shows a selected entry's details.
    pub(super) transcript_browsing: bool,
    /// Scroll state for the browsing view (used when transcript_browsing is true).
    /// Reset to follow=false, position=0 when focused item changes.
    pub(super) browsing_view_scroll: ratatui_widget_scrolling::ScrollState,
    /// When set, detail view footer shows "Copied to clipboard ✓" until this instant.
    pub(super) copy_notice_until: Option<std::time::Instant>,
    /// Set to true whenever transcript_focus changes so draw() scrolls once
    /// to keep the newly focused item visible, then clears it.
    pub(super) scroll_to_focused_item: bool,
    /// When true, render timestamps in UTC instead of local time.
    /// Always false in production; set to true in tests so snapshot
    /// output is timezone-independent.
    pub(super) use_utc_timestamps: bool,
    /// Child-session state is intentionally separate from the root transcript
    /// so nested events cannot mutate root busy, input, or streaming state.
    pub(super) monitored_sessions: HashMap<MonitoredSessionKey, MonitoredSessionState>,
    /// Fullscreen child-session drilldown stack. The last invocation is displayed.
    pub(super) subagent_view_stack: Vec<SubAgentView>,
}

#[derive(Clone, Copy)]
pub(super) struct RenderEntryState {
    pub skip_cache: bool,
    pub spinner_index: usize,
}

impl RenderEntryState {
    pub(super) fn new(skip_cache: bool, spinner_index: usize) -> Self {
        Self {
            skip_cache,
            spinner_index,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MonitoredSessionKey {
    pub cluster: String,
    pub agent: String,
    pub session_id: String,
}

impl MonitoredSessionKey {
    pub fn storage_key(&self) -> String {
        harnx_core::session_identity::session_key(Some(&self.agent), &self.session_id)
    }
}

#[derive(Clone, Debug)]
pub(super) struct SubAgentView {
    pub key: MonitoredSessionKey,
    pub status: SubAgentStatus,
    pub progress: Option<SubAgentInvocationProgress>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubAgentStatus {
    Running,
    Cancelling,
    Cancelled,
    Unconfirmed,
    Completed,
    Failed,
}

impl SubAgentStatus {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
            Self::Unconfirmed => "unconfirmed",
            Self::Completed => "done",
            Self::Failed => "failed",
        }
    }

    pub(super) fn from_progress(status: SubAgentProgressStatus) -> Self {
        match status {
            SubAgentProgressStatus::Running => Self::Running,
            SubAgentProgressStatus::Cancelling => Self::Cancelling,
            SubAgentProgressStatus::Cancelled => Self::Cancelled,
            SubAgentProgressStatus::Unconfirmed => Self::Unconfirmed,
            SubAgentProgressStatus::Done => Self::Completed,
            SubAgentProgressStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubAgentInvocationProgress {
    pub snapshot: SubAgentProgress,
    received_at: std::time::Instant,
}

impl SubAgentInvocationProgress {
    pub(super) fn new(snapshot: SubAgentProgress) -> Self {
        Self {
            snapshot,
            received_at: std::time::Instant::now(),
        }
    }

    pub(super) fn elapsed_ms(&self) -> u64 {
        if self.snapshot.status != SubAgentProgressStatus::Running {
            return self.snapshot.elapsed_ms;
        }
        self.snapshot.elapsed_ms.saturating_add(
            u64::try_from(self.received_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        )
    }
}

pub(super) struct MonitoredSessionState {
    pub live_events: harnx_runtime::nats_event_sink::LiveEventState,
    pub invocation_id: Option<String>,
    pub execution_id: Option<String>,
    pub transcript: Vec<TranscriptItem>,
    pub status: SubAgentStatus,
    pub transcript_focus: Option<usize>,
    pub scroll: ratatui_widget_scrolling::ScrollState,
    pub scroll_to_focused_item: bool,
    pub streaming_open: bool,
}

impl MonitoredSessionState {
    pub(super) fn new(status: SubAgentStatus) -> Self {
        let mut scroll = ratatui_widget_scrolling::ScrollState::new();
        scroll.follow = true;
        Self {
            transcript: Vec::new(),
            execution_id: None,
            live_events: Default::default(),
            invocation_id: None,
            status,
            transcript_focus: None,
            scroll,
            scroll_to_focused_item: false,
            streaming_open: false,
        }
    }
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
pub enum ToolCallBody {
    /// YAML rendering of the raw tool-call arguments. Displayed verbatim.
    Yaml(String),
    /// Rendered MCP `call_template` output. Each line is treated as inline
    /// markdown (`**bold**`, `*italic*`, `` `code` ``).
    Markdown(String),
}

/// View mode for tool confirmation modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfirmView {
    /// Rendered call_template output (if available).
    Template,
    /// Raw YAML rendering of arguments.
    RawYaml,
}

/// State for tool confirmation modal (`ModalState::ConfirmToolUse`).
/// Extracted into a struct to avoid `large_enum_variant` warning.
#[allow(dead_code)]
pub(super) struct ConfirmToolUseState {
    /// Full arguments as received from the worker (no truncation).
    pub arguments: serde_json::Value,
    /// Tool name for display.
    pub tool_name: String,
    /// Reason from the hook, if any.
    pub reason: Option<String>,
    /// Origin session that made the tool call.
    pub session_id: String,
    /// Origin cluster for the session.
    pub cluster: String,
    /// Tool call ID from the request, if provided.
    pub tool_call_id: Option<String>,
    /// Stable UUID generated once at modal open, used for idempotent enqueue.
    pub submission_id: String,
    /// Confirmation route captured for the origin session at modal open.
    pub confirmation_route: Option<ToolConfirmationRouteHandle>,
    /// Cancels the detached append when this confirmation is dismissed.
    pub submission_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Last append error. Kept with the draft so the user can retry.
    pub submission_error: Option<String>,
    /// Current view mode: Template if available, else RawYaml.
    pub view: ConfirmView,
    /// Whether the tool has a call_template (enables Ctrl+F toggle).
    pub has_template: bool,
    /// Pre-rendered template text (if has_template is true).
    pub template_text: Option<String>,
    /// Scroll state for the arguments preview.
    pub scroll: ratatui_widget_scrolling::ScrollState,
    /// Optional message textarea for user-provided context.
    pub message: TextArea<'static>,
    /// When the modal was first shown (idle gate).
    pub opened_at: Instant,
    /// Time of last keypress in this modal (idle gate).
    pub last_key_at: Instant,
    /// True while an async enqueue is in flight (blocks double-submit).
    pub submitting: bool,
}

/// Modal dialog state for destructive action confirmations.
/// Used by D5 for delete/rewind confirmations.
#[allow(dead_code)]
pub(super) enum ModalState {
    /// Confirmation for deleting one or more transcript entries.
    ConfirmDelete { from: usize, to: usize },
    /// Confirmation for rewinding session to a specific entry.
    ConfirmRewind {
        seq: usize,
        user_text: Option<String>,
    },
    /// Confirmation for a tool call gated by a `PreToolUse` "ask" hook.
    /// The reply channel lives in `App::pending_confirm_reply`.
    ConfirmToolUse(Box<ConfirmToolUseState>),
    /// Agent is still working when user tries to exit.
    ConfirmExit {
        worker_state: ExitWorkerState,
        phase: ExitPhase,
    },
    /// Agent selection
    AgentPicker {
        agents: Vec<String>,
        selected: usize,
        /// Live filter string typed by the user; empty means show all.
        query: String,
    },
    /// Session selection.
    /// The agent is always activated before this picker is shown.
    /// `origin_agent` / `origin_session` carry the pre-activation agent and
    /// session names so that `reconcile_transcript_after_command` can detect
    /// the full agent+session transition when the picker is eventually confirmed
    /// or dismissed via Esc.
    SessionPicker {
        sessions: Vec<SessionMeta>,
        selected: usize,
        /// Agent name *before* the picker flow started (pre-activation).
        origin_agent: Option<String>,
        /// Session id *before* the picker flow started (pre-activation).
        origin_session: Option<String>,
        /// Error message when remote session fetch failed. Displayed visibly
        /// in the picker so users know the cluster was unreachable, rather
        /// than seeing an empty list and assuming no sessions exist.
        error: Option<String>,
    },
}

impl std::fmt::Debug for ModalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfirmDelete { from, to } => f
                .debug_struct("ConfirmDelete")
                .field("from", from)
                .field("to", to)
                .finish(),
            Self::ConfirmRewind { seq, user_text } => f
                .debug_struct("ConfirmRewind")
                .field("seq", seq)
                .field("user_text", user_text)
                .finish(),
            Self::ConfirmToolUse(state) => f
                .debug_struct("ConfirmToolUse")
                .field("arguments", &state.arguments)
                .field("tool_name", &state.tool_name)
                .field("reason", &state.reason)
                .field("session_id", &state.session_id)
                .field("cluster", &state.cluster)
                .field("tool_call_id", &state.tool_call_id)
                .field("submission_id", &state.submission_id)
                .field("view", &state.view)
                .field("has_template", &state.has_template)
                .field("template_text", &state.template_text)
                .field("opened_at", &state.opened_at)
                .field("last_key_at", &state.last_key_at)
                .field("submitting", &state.submitting)
                .finish_non_exhaustive(),
            Self::ConfirmExit {
                worker_state,
                phase,
            } => f
                .debug_struct("ConfirmExit")
                .field("worker_state", worker_state)
                .field("phase", phase)
                .finish(),
            Self::AgentPicker {
                agents,
                selected,
                query,
            } => f
                .debug_struct("AgentPicker")
                .field("agents", agents)
                .field("selected", selected)
                .field("query", query)
                .finish(),
            Self::SessionPicker {
                sessions,
                selected,
                origin_agent,
                origin_session,
                error,
            } => f
                .debug_struct("SessionPicker")
                .field("sessions", sessions)
                .field("selected", selected)
                .field("origin_agent", origin_agent)
                .field("origin_session", origin_session)
                .field("error", error)
                .finish(),
        }
    }
}

impl Clone for ModalState {
    fn clone(&self) -> Self {
        match self {
            Self::ConfirmDelete { from, to } => Self::ConfirmDelete {
                from: *from,
                to: *to,
            },
            Self::ConfirmRewind { seq, user_text } => Self::ConfirmRewind {
                seq: *seq,
                user_text: user_text.clone(),
            },
            Self::ConfirmToolUse(state) => {
                let state = state.as_ref();
                Self::ConfirmToolUse(Box::new(ConfirmToolUseState {
                    arguments: state.arguments.clone(),
                    tool_name: state.tool_name.clone(),
                    reason: state.reason.clone(),
                    session_id: state.session_id.clone(),
                    cluster: state.cluster.clone(),
                    tool_call_id: state.tool_call_id.clone(),
                    submission_id: state.submission_id.clone(),
                    confirmation_route: state.confirmation_route.clone(),
                    submission_cancel: Arc::clone(&state.submission_cancel),
                    submission_error: state.submission_error.clone(),
                    view: state.view,
                    has_template: state.has_template,
                    template_text: state.template_text.clone(),
                    // ScrollState doesn't implement Clone, so create a new one
                    // preserving only the position and follow state
                    scroll: {
                        let mut new_scroll = ratatui_widget_scrolling::ScrollState::new();
                        new_scroll.position = state.scroll.position;
                        new_scroll.follow = state.scroll.follow;
                        new_scroll.last_max_position = state.scroll.last_max_position;
                        new_scroll
                    },
                    message: state.message.clone(),
                    opened_at: state.opened_at,
                    last_key_at: state.last_key_at,
                    submitting: state.submitting,
                }))
            }
            Self::ConfirmExit {
                worker_state,
                phase,
            } => Self::ConfirmExit {
                worker_state: *worker_state,
                phase: *phase,
            },
            Self::AgentPicker {
                agents,
                selected,
                query,
            } => Self::AgentPicker {
                agents: agents.clone(),
                selected: *selected,
                query: query.clone(),
            },
            Self::SessionPicker {
                sessions,
                selected,
                origin_agent,
                origin_session,
                error,
            } => Self::SessionPicker {
                sessions: sessions.clone(),
                selected: *selected,
                origin_agent: origin_agent.clone(),
                origin_session: origin_session.clone(),
                error: error.clone(),
            },
        }
    }
}

impl PartialEq for ModalState {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ConfirmDelete {
                    from: a_from,
                    to: a_to,
                },
                Self::ConfirmDelete {
                    from: b_from,
                    to: b_to,
                },
            ) => a_from == b_from && a_to == b_to,
            (
                Self::ConfirmRewind {
                    seq: a_seq,
                    user_text: a_user_text,
                },
                Self::ConfirmRewind {
                    seq: b_seq,
                    user_text: b_user_text,
                },
            ) => a_seq == b_seq && a_user_text == b_user_text,
            (Self::ConfirmToolUse(a), Self::ConfirmToolUse(b)) => {
                a.arguments == b.arguments
                    && a.tool_name == b.tool_name
                    && a.reason == b.reason
                    && a.session_id == b.session_id
                    && a.cluster == b.cluster
                    && a.tool_call_id == b.tool_call_id
                    && a.submission_id == b.submission_id
                    && a.submission_error == b.submission_error
                    && a.view == b.view
                    && a.has_template == b.has_template
                    && a.template_text == b.template_text
                    && a.opened_at == b.opened_at
                    && a.last_key_at == b.last_key_at
                    && a.submitting == b.submitting
                // scroll and message intentionally excluded from equality check
            }
            (
                Self::ConfirmExit {
                    worker_state: a_worker_state,
                    phase: a_phase,
                },
                Self::ConfirmExit {
                    worker_state: b_worker_state,
                    phase: b_phase,
                },
            ) => a_worker_state == b_worker_state && a_phase == b_phase,
            (
                Self::AgentPicker {
                    agents: a_agents,
                    selected: a_selected,
                    query: a_query,
                },
                Self::AgentPicker {
                    agents: b_agents,
                    selected: b_selected,
                    query: b_query,
                },
            ) => a_agents == b_agents && a_selected == b_selected && a_query == b_query,
            (
                Self::SessionPicker {
                    sessions: a_sessions,
                    selected: a_selected,
                    origin_agent: a_origin_agent,
                    origin_session: a_origin_session,
                    error: a_error,
                },
                Self::SessionPicker {
                    sessions: b_sessions,
                    selected: b_selected,
                    origin_agent: b_origin_agent,
                    origin_session: b_origin_session,
                    error: b_error,
                },
            ) => {
                a_sessions == b_sessions
                    && a_selected == b_selected
                    && a_origin_agent == b_origin_agent
                    && a_origin_session == b_origin_session
                    && a_error == b_error
            }
            _ => false,
        }
    }
}

impl ModalState {
    pub(super) fn simple_confirmation_prompt(&self) -> Option<String> {
        match self {
            Self::ConfirmDelete { from, to } if from == to => {
                Some(format!("Delete entry {from}? [y/N]"))
            }
            Self::ConfirmDelete { from, to } => Some(format!("Delete entries {from}–{to}? [y/N]")),
            Self::ConfirmRewind { seq, .. } => Some(format!("Rewind to entry {seq}? [y/N]")),
            _ => None,
        }
    }

    /// Return the subset of agents matching the current query (case-insensitive
    /// substring). Returns all agents when the query is empty.
    pub(super) fn filtered_agents(agents: &[String], query: &str) -> Vec<String> {
        let q = query.to_lowercase();
        agents
            .iter()
            .filter(|a| q.is_empty() || a.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }
}

pub(crate) type RenderedCache = Option<(u16, bool, bool, bool, RenderedEntry)>;

#[derive(Clone, Debug)]
pub enum TranscriptItem {
    SourceHeading(AgentSource),
    SystemText(String),
    UserText {
        text: String,
        seq: Option<usize>,
        timestamp: Option<DateTime<Utc>>,
    },
    AssistantText {
        text: String,
        seq: Option<usize>,
        timestamp: Option<DateTime<Utc>>,
        rendered_cache: RenderedCache,
    },
    ErrorText(String),
    ThoughtText(String),
    /// Tool result body — the full multi-line text extracted from the
    /// MCP `CallToolResult`. Rendered through `markdown_lines` (with a
    /// dim base style) so block-level markdown like fenced diffs and
    /// inline emphasis from a `result_template` both display correctly.
    ToolResultMarkdown {
        text: String,
        /// Full, untruncated, all-audience tool output ("what the agent sees").
        /// `Some` only when it differs from `text` (i.e. there is genuinely more
        /// than the collapsed user-facing view). Rendered by the detail overlay.
        full_detail: Option<String>,
        rendered_cache: RenderedCache,
    },
    StatusLine(String),
    CompactionMarker {
        text: String,
        summary_text: String,
        from_seq: Option<usize>,
        to_seq: Option<usize>,
        detail_text: String,
    },
    Plan(Vec<PlanEntry>),
    ToolCall {
        tool_name: String,
        body: Option<ToolCallBody>,
        seq: Option<usize>,
        timestamp: Option<DateTime<Utc>>,
        /// Tool-call ID from `ToolEvent::Started { id, .. }`.
        /// Used to correlate completion/failure events with the correct running tool call
        /// when tools execute concurrently and may complete out of order.
        id: Option<String>,
        /// Monotonic start anchor for live elapsed display.
        /// Uses `Instant` instead of `DateTime<Utc>` to avoid wall-clock skew.
        start_anchor: std::time::Instant,
        /// Final duration in milliseconds, set when the tool completes.
        /// When `Some`, the tool has finished and the timer should show the
        /// frozen final value instead of ticking.
        final_elapsed_ms: Option<u64>,
        rendered_cache: RenderedCache,
    },
    AttachmentHeader(String),
    AttachmentItem(String),
    AttachmentPreviewLine(String),
    MutationNotice(String),
    /// Compact, selectable link to a separately monitored child transcript.
    SubAgentSession {
        key: MonitoredSessionKey,
        status: SubAgentStatus,
        invocation_id: Option<String>,
        progress: Option<SubAgentInvocationProgress>,
    },
}

impl TranscriptItem {
    /// Text to show for a `ToolResultMarkdown` in the detail overlay: the
    /// full, untruncated `full_detail` when present, else the collapsed
    /// `text`. Returns `None` for other variants.
    pub(crate) fn tool_result_detail_text(&self) -> Option<&str> {
        match self {
            TranscriptItem::ToolResultMarkdown {
                text, full_detail, ..
            } => Some(full_detail.as_deref().unwrap_or(text)),
            _ => None,
        }
    }

    /// Get the seq number of this transcript item, if available.
    pub(crate) fn seq(&self) -> Option<usize> {
        match self {
            TranscriptItem::UserText { seq, .. } => *seq,
            TranscriptItem::AssistantText { seq, .. } => *seq,
            TranscriptItem::ToolCall { seq, .. } => *seq,
            _ => None,
        }
    }

    /// Whether this item can be focused with arrow keys.
    pub(crate) fn is_navigable(&self) -> bool {
        // ToolResultMarkdown is intentionally excluded: a tool result is always
        // paired with its preceding ToolCall.  Focusing the ToolCall is sufficient
        // — get_message_range_yaml auto-expands to include the paired result via
        // adjust_range_for_tool_pairs.  Allowing focus on a bare ToolResultMarkdown
        // would show an incomplete view and break navigation semantics.
        matches!(
            self,
            TranscriptItem::UserText { .. }
                | TranscriptItem::AssistantText { .. }
                | TranscriptItem::ToolCall { .. }
                | TranscriptItem::CompactionMarker { .. }
                | TranscriptItem::SubAgentSession { .. }
        )
    }
}

impl Default for App {
    fn default() -> Self {
        Self {
            transcript: Vec::new(),
            input: TextArea::default(),
            spinner_index: 0,
            should_quit: false,
            llm_busy: false,
            scroll_state: ratatui_widget_scrolling::ScrollState::new(),
            streaming_open: false,
            main_streamed_text_idx: None,
            cache_valid_width: None,
            last_ui_output_source: None,
            pending_thought_source: None,
            pending_thought_text: String::new(),
            pending_tool_seq: None,
            pending_message: None,
            completions: Vec::new(),
            completion_index: 0,
            completion_prefix: String::new(),
            completion_suffix: String::new(),
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            history_preview: false,
            attachments: Vec::new(),
            attachment_dir: None,
            paste_count: 0,
            last_known_input_width: 0,
            show_sequence_numbers: false,
            show_timestamps: false,
            transcript_focus: None,
            transcript_selection_anchor: None,
            modal: None,
            pending_confirm_reply: None,
            pending_confirm_id: None,
            current_session_unread: false,
            detail_view_scroll: ratatui_widget_scrolling::ScrollState::new(),
            detail_view_open: false,
            detail_view_text: None,
            detail_view_entry: None,
            detail_view_title: None,
            transcript_browsing: false,
            browsing_view_scroll: ratatui_widget_scrolling::ScrollState::new(),
            copy_notice_until: None,
            scroll_to_focused_item: false,
            use_utc_timestamps: false,
            monitored_sessions: HashMap::new(),
            subagent_view_stack: Vec::new(),
        }
    }
}

impl App {
    /// Returns the (min, max) indices of the current transcript selection.
    /// Used by render_detail_view to determine which entries to display.
    pub(super) fn selected_transcript_range(&self) -> (usize, usize) {
        let f = self.transcript_focus.unwrap_or(0);
        let a = self.transcript_selection_anchor.unwrap_or(f);
        (f.min(a), f.max(a))
    }
}

pub(crate) enum ToolConfirmationEvent {
    /// A worker-side `PreToolUse` hook asked for confirmation via NATS. The
    /// async task waits on `reply`; the main loop shows a modal and sends the decision back.
    Show {
        confirmation_id: u64,
        /// Canonical session storage key captured by the route owner.
        origin_session_id: String,
        /// NATS cluster captured by the route owner.
        cluster: String,
        /// Tool call ID from the request, if provided.
        tool_call_id: Option<String>,
        /// Tool name for display.
        tool_name: String,
        /// Full arguments as received from the worker (no truncation).
        arguments: Box<serde_json::Value>,
        /// Reason from the hook, if any.
        reason: Option<String>,
        reply: ToolConfirmationReply,
    },
    /// The remote request stopped waiting. Dismiss the matching modal without
    /// disturbing a newer confirmation or another modal.
    Dismiss { confirmation_id: u64 },
}

pub(super) struct SubAgentSnapshot {
    pub invocation_id: Option<String>,
    pub transcript: Vec<TranscriptItem>,
    pub status: SubAgentStatus,
}

pub(crate) enum TuiEvent {
    /// Local commands/startup only. Never used by a live NATS follower.
    LocalAgent(harnx_core::event::AgentEvent),
    Agent {
        task: AbortSignal,
        stamp: crate::event_isolation::EventStamp,
        event: harnx_core::event::AgentEvent,
    },
    /// The locally-owned prompt task has exited. `Turn::Ended` normally closes
    /// busy state; this is the fallback for a lossy advisory or setup failure.
    PromptTaskFinished {
        task: AbortSignal,
        error: Option<String>,
    },
    /// Shared activity observed directly from the session fan-out stream.
    SessionActivity {
        historical: bool,
        stamp: crate::event_isolation::EventStamp,
        session_id: String,
        cluster: String,
        active: bool,
    },
    /// Agent output observed from another frontend on the selected session.
    SessionAgent {
        stamp: crate::event_isolation::EventStamp,
        historical: bool,
        session_id: String,
        cluster: String,
        event: harnx_core::event::AgentEvent,
    },
    /// Durable child history loaded subscribe-first before live forwarding.
    SubAgentSessionSnapshot {
        key: MonitoredSessionKey,
        snapshot: SubAgentSnapshot,
    },
    /// Live advisory belonging exclusively to a monitored child session.
    SubAgentSessionEvent {
        stamp: crate::event_isolation::EventStamp,
        key: MonitoredSessionKey,
        event: harnx_core::event::AgentEvent,
    },
    /// A lease watchdog confirmed the loss of this specific invocation.
    SubAgentInvocationFailed {
        key: MonitoredSessionKey,
        invocation_id: String,
    },
    /// Intermediate tool round completed; retained for queued-message tests.
    #[allow(dead_code)]
    ToolRoundComplete,
    /// Prompt task consumed pending message; retained for queued-message tests.
    #[allow(dead_code)]
    PendingMessageConsumed(PendingMessage),
    ToolConfirmation(ToolConfirmationEvent),
    ToolConfirmationEnqueueFinished {
        confirmation_id: u64,
        decision: crate::tool_confirmation::ConfirmDecision,
        result: crate::tool_confirmation::ConfirmationEnqueueResult,
    },
    /// Another frontend marked the session as read; TUI should update its cached unread state.
    SessionReadInvalidation {
        session_id: String,
    },
    /// Periodic reconcile event to refresh session list (emitted by main loop when picker is open).
    RefreshSessionList,
}
