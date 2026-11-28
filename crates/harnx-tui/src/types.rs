use harnx_core::api_types::CompletionTokenUsage;
use harnx_core::event::{
    AgentSource, PlanEntry, SubAgentProgress, SubAgentProgressStatus, ToolKind, ToolLocation,
    ToolStatus,
};
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

#[cfg(test)]
pub(super) type TestCidResolveFn = Arc<
    dyn Fn(
            String,
        )
            -> Pin<Box<dyn Future<Output = anyhow::Result<harnx_blob_store::ResolvedBlob>> + Send>>
        + Send
        + Sync,
>;
#[cfg(test)]
pub(super) type TestDetachedOpenFn =
    Arc<dyn Fn(&std::path::Path) -> anyhow::Result<()> + Send + Sync>;
#[cfg(test)]
pub(super) type TestClipboardFn = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;
#[cfg(test)]
pub(super) type TestCommandFn = Arc<dyn Fn(&str) + Send + Sync>;
#[cfg(test)]
pub(super) type TestHistoryLoadFn = Arc<
    dyn Fn(
            GlobalConfig,
            crate::history_work::RebuildMode,
            AbortSignal,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<TranscriptItem>>> + Send>>
        + Send
        + Sync,
>;
#[cfg(test)]
pub(super) type TestHistoryCommandGateFn = Arc<
    dyn Fn(String, AbortSignal) -> Pin<Box<dyn Future<Output = AbortSignal> + Send>> + Send + Sync,
>;
#[cfg(test)]
pub(super) type TestPickerSessionsFn = Arc<
    dyn Fn(
            GlobalConfig,
            AbortSignal,
        ) -> Pin<Box<dyn Future<Output = (Vec<SessionMeta>, Option<String>)> + Send>>
        + Send
        + Sync,
>;
#[cfg(test)]
pub(super) type TestInfoOverlayFn = Arc<
    dyn Fn(
            GlobalConfig,
            Vec<String>,
            crate::input_commands::InfoOverlayType,
            AbortSignal,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>
        + Send
        + Sync,
>;

/// Target identity `(cluster, agent)` owning a background session-picker list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PickerTarget {
    pub cluster: String,
    pub agent: Option<String>,
}

/// Identity of the TUI's currently displayed session target for history
/// rebuild/repair fencing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HistoryTarget {
    pub session_id: Option<String>,
    pub agent: Option<String>,
    pub cluster: String,
}

/// Full durable destination identity (`HistoryTarget` plus canonical session
/// storage key) used to fence unsettled writes and preserve navigation
/// replacement intent without collapsing distinct targets that share short IDs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HistoryDestination {
    pub target: HistoryTarget,
    pub storage_key: Option<String>,
}

/// Target identity and view revision that owns the transcript rows currently
/// displayed in `app.transcript`. Confirmations and queued durable row
/// mutations bind to this exact value and are rejected if navigation,
/// retirement, or a same-session incarnation/rebuild advances past it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptActionTarget {
    pub cluster: String,
    pub storage_key: Option<String>,
    pub target: HistoryTarget,
    pub view_revision: u64,
}

/// Ownership state for `app.transcript` row-derived actions and target-bound
/// session history commands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptViewOwnership {
    pub view_revision: u64,
    pub active_binding: Option<TranscriptActionTarget>,
    pub active_destination: HistoryDestination,
    /// Full durable destination of a coordinate-invalidating history command
    /// (`invalidates_transcript_coordinates`) whose write is in flight. Set
    /// when the command starts and cleared only by actual settlement
    /// (`settle_history_mutation`) — never on navigation abort or caller
    /// cancellation, since a failed or cancelled write may still have
    /// landed in durable history. Blocks same-destination rebuilds while
    /// reads for other destinations proceed. Regression:
    /// `src/tests/unsettled_write_navigation_tests.rs` (#2922).
    pub unsettled_write_destination: Option<HistoryDestination>,
    /// Destination a target switch navigated to while the display may still
    /// hold the previous destination's foreign rows. Set on every switched
    /// navigation — not only returns to a destination with a prior write.
    /// Replacement-on-error semantics must survive any superseding read (a
    /// failed reconnect repair or paced retry must not downgrade to
    /// preserve-stale), or the old destination's rows can stay on screen
    /// under the new header. Cleared only once those foreign rows are
    /// actually removed — by a committed rebuild, an initial-error
    /// replacement, or an explicit handoff; rejected and mutation-drift
    /// snapshots leave the intent pending. Regression:
    /// `src/tests/navigation_failure_policy_tests.rs` (#2922).
    pub unresolved_navigation_target: Option<HistoryDestination>,
    pub session_fence: SessionCommandFence,
}

impl TranscriptViewOwnership {
    pub(crate) fn initial() -> Self {
        Self {
            view_revision: 1,
            session_fence: SessionCommandFence::initial(),
            ..Self::default()
        }
    }
}

/// Submission target identity for session-wide history commands (`.empty
/// session`, `.reset session`, `.reset repl`, `.compact session`,
/// `.edit session`), binding them to the submitting session without requiring
/// transcript row bindings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionCommandTarget {
    pub target: HistoryTarget,
    pub storage_key: Option<String>,
    pub target_epoch: u64,
}

/// Fence state for target-bound history commands (row and session-wide):
/// tracks the monotonic `target_epoch` bumped on every real target switch and
/// the currently running bound command's captured submission target + epoch so
/// stale completions from a prior target or epoch cannot affect the current
/// destination's work or transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionCommandFence {
    pub target_epoch: u64,
    pub running_bound_target: Option<SessionCommandTarget>,
}

impl SessionCommandFence {
    pub(crate) fn initial() -> Self {
        Self {
            target_epoch: 1,
            ..Self::default()
        }
    }
}

/// Entry in `Tui::history_command_queue`: durable transcript mutations carry
/// the `TranscriptActionTarget` captured from the displayed rows so a prior
/// target switch or view revision change rejects them before execution;
/// session-wide history commands carry `SessionCommandTarget` captured at
/// submission without requiring row bindings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueuedHistoryCommand {
    pub line: String,
    pub expected_target: Option<TranscriptActionTarget>,
    pub expected_session: Option<SessionCommandTarget>,
}

/// Payload for `TuiEvent::HistoryRebuildFinished`. Bundled in a struct so the
/// completion handler and event variant stay compact and within parameter caps.
pub(crate) struct HistoryRebuildCompletion {
    pub seq: u64,
    pub target: HistoryTarget,
    pub attachment_gen: u64,
    pub refresh_rev: u64,
    pub mutation_fence: u64,
    pub live_stamp: Option<crate::event_isolation::EventStamp>,
    pub preserved_from: usize,
    pub mode: crate::history_work::RebuildMode,
    pub failure_policy: crate::history_work::RebuildFailurePolicy,
    pub result: Result<Vec<TranscriptItem>, String>,
}

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
    pub(super) interactions: crate::interaction_confirmation::InteractionClient,
    pub(super) surfaces: crate::surfaces::SurfaceClient,
    /// Deterministic enqueue seam for confirmation orchestration tests.
    #[cfg(test)]
    pub(super) confirmation_enqueue_override:
        Option<crate::tool_confirmation::TestConfirmationEnqueueFn>,
    #[cfg(test)]
    pub(super) cid_resolve_override: Option<TestCidResolveFn>,
    #[cfg(test)]
    pub(super) detached_open_override: Option<TestDetachedOpenFn>,
    #[cfg(test)]
    pub(super) copy_to_clipboard_override: Option<TestClipboardFn>,
    #[cfg(test)]
    pub(super) command_override: Option<TestCommandFn>,
    #[cfg(test)]
    pub(super) history_load_override: Option<TestHistoryLoadFn>,
    #[cfg(test)]
    pub(super) history_command_gate: Option<TestHistoryCommandGateFn>,
    #[cfg(test)]
    pub(super) picker_sessions_override: Option<TestPickerSessionsFn>,
    #[cfg(test)]
    pub(super) info_overlay_override: Option<TestInfoOverlayFn>,
    #[cfg(test)]
    pub(super) history_repair_retry_delay_override: Option<Duration>,
    /// Sessions whose durable text append succeeded but whose worker activation
    /// must be retried without submitting the text as a second user message.
    pub(super) pending_remote_activations: HashSet<(String, String)>,
    /// Session whose shared NATS turn activity is currently being observed.
    pub(super) session_activity_target: Option<(String, String)>,
    /// Background subscription that lets this TUI react to turns submitted by
    /// another client attached to the same session.
    pub(super) session_activity_handle: Option<JoinHandle<()>>,
    /// Root session whose nested sub-agent monitors belong to. Changing the
    /// root aborts every child monitor, drops their retained views and
    /// replaces `live_events`, whose cancel fence came from the old root's log.
    pub(super) subagent_monitor_root: Option<(String, String)>,
    /// Independent live subscriptions for child sessions.
    pub(super) subagent_monitor_handles: HashMap<MonitoredSessionKey, JoinHandle<()>>,
    /// Set when a durable transcript rebuild may have introduced child rows.
    /// The next monitor sync consumes it instead of rescanning every frame.
    pub(super) subagent_rows_dirty: bool,
    /// Sequence of the most recently scheduled background history rebuild;
    /// completions carrying anything else are stale and discarded.
    pub(super) history_rebuild_seq: u64,
    /// Abort signal of the in-flight background history rebuild, superseded
    /// (set) when a newer rebuild is scheduled, cancelled by Ctrl-C, or
    /// taken (cleared) when the rebuild no longer has an in-flight load to
    /// cancel — completion applied, target discarded, or cancel requested.
    pub(super) history_rebuild_abort: Option<AbortSignal>,
    /// Task handle of the in-flight background history rebuild, aborted on
    /// supersession, Ctrl-C, or Drop so at most one load runs at a time.
    pub(super) history_rebuild_handle: Option<JoinHandle<()>>,
    /// Attachment generation of the current session monitor; increments on
    /// monitor attach/detach and on same-session stream reconnections so a
    /// snapshot read from an earlier attachment cannot overwrite newer state.
    pub(super) session_attachment_gen: u64,
    /// True after the current monitor has reported its first stream attach;
    /// subsequent `SessionStreamAttached` events on the same monitor are
    /// same-session reconnects and bump `session_attachment_gen`.
    pub(super) session_stream_attached: bool,
    /// Monotonic revision of transcript snapshot scheduling/application.
    pub(super) history_refresh_rev: u64,
    /// Monotonic counter of live transcript mutations (messages, tool rows,
    /// command output, snapshots). Used to reject stale repair/refresh reads
    /// that started before newer live rows arrived.
    pub(super) transcript_mutation_seq: u64,
    /// Target whose initial history load failed (or whose repair was discarded
    /// after concurrent live edits) and still needs background repair.
    pub(super) history_repair_target: Option<HistoryTarget>,
    /// Whether pending `RepairInitial` reloads should wrap items with the
    /// initial welcome/agent header (true for startup/target-switch/PreserveTail
    /// repair; false when repairing a failed `ReplaceAll` compaction/handoff).
    pub(super) history_repair_uses_wrapper: bool,
    /// Current paced-retry delay for pending history repair (resets on success
    /// or target switch).
    pub(super) history_repair_retry_delay: Duration,
    /// Handle of the scheduled paced-retry timer for pending history repair.
    pub(super) history_repair_retry_handle: Option<JoinHandle<()>>,
    /// A backgrounded history command (session switch / transcript
    /// mutation) is in flight; its finish event will arrive on the loop.
    pub(super) history_command_running: bool,
    /// Abort signal of the in-flight backgrounded history command. Each
    /// command gets its own signal: Enter's `abort_signal.reset()` must not
    /// un-abort an in-flight command (design 9 cancellation bug).
    pub(super) history_command_abort: Option<AbortSignal>,
    /// History commands submitted while one is in flight; started in
    /// order when the running one finishes, preserving dot-command order.
    pub(super) history_command_queue: std::collections::VecDeque<QueuedHistoryCommand>,
    /// Abort signal of the single in-flight background `.set title` rename;
    /// retired on Ctrl-C or any target switch.
    pub(super) title_rename_abort: Option<AbortSignal>,
    /// Fence for background `.set title` renames: incremented on spawn,
    /// cancellation, or target switch so stale completions never update chrome.
    pub(super) title_rename_seq: u64,
    /// Target identity and view revision owning the currently displayed
    /// transcript rows; `active_binding` is `None` while prior-target rows
    /// are retired during navigation or a pending mutation reload.
    pub(crate) transcript_view_ownership: TranscriptViewOwnership,
    /// Abort signal of the in-flight background info-overlay load
    /// (`.info session` / `.dump session`); superseded by a newer request
    /// and cancelled by Ctrl-C.
    pub(super) info_overlay_abort: Option<AbortSignal>,
    /// Fence for background info-overlay loads: each spawn, cancellation, or
    /// target switch bumps the seq so stale completions cannot clear a newer
    /// slot or open an overlay after Ctrl-C or navigation.
    pub(super) info_overlay_seq: u64,
    /// Abort signal of the in-flight background session-picker refresh;
    /// superseded by a newer refresh and cancelled by Ctrl-C.
    pub(super) picker_refresh_abort: Option<AbortSignal>,
    /// Fence for background session-picker refreshes: bumped on each spawn,
    /// cancellation, or picker dismissal/reopen.
    pub(super) picker_refresh_seq: u64,
    /// Monotonic incarnation of the active `SessionPicker` modal; prevents
    /// queued completions from an earlier picker from populating a reopened one.
    pub(super) picker_incarnation: u64,
    /// Target `(cluster, agent)` of the currently active picker refresh slot.
    pub(super) picker_refresh_target: Option<PickerTarget>,
    /// Abort signal of the in-flight background "New session" reservation
    /// started from the session picker; superseded by a newer reservation
    /// and cancelled by Ctrl-C.
    pub(super) new_session_reservation_abort: Option<AbortSignal>,
    /// Fence for background "New session" reservations: each spawn bumps the
    /// seq and carries it in its completion event, so a stale completion
    /// (superseded or cancelled job) can never clear the newer job's slot,
    /// switch the session, or clobber the modal.
    pub(super) new_session_reservation_seq: u64,
    /// Abort signal of the in-flight background picker unread toggle
    /// (`u`/`U` in the session picker); superseded by a newer toggle and
    /// cancelled by Ctrl-C.
    pub(super) picker_unread_toggle_abort: Option<AbortSignal>,
    /// Fence for background picker unread toggles: a stale completion must
    /// not trigger a refresh for a view that has moved on.
    pub(super) picker_unread_toggle_seq: u64,
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
    /// Presentation label; explicit files and pastes can share a name. Target
    /// a selected attachment by its index or storage path when removing it.
    pub(super) display_name: String,
}

#[derive(Clone)]
pub(crate) struct PendingMessage {
    pub(super) text: String,
    pub(super) attachments: Vec<Attachment>,
    pub(super) attachment_dir: Option<PathBuf>,
    pub(super) paste_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CidDocView {
    pub(super) url: String,
    pub(super) title: String,
    pub(super) text: String,
    pub(super) links: Vec<crate::markdown_render::ExtractedLink>,
    pub(super) focused_link: Option<usize>,
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
    /// Interleaving output closes aggregation after projecting the run's links.
    /// Its tracked row can still receive the canonical Final until a tool round ends.
    pub(super) streaming_open: bool,
    /// Current source's streamed row, retained across notices until its Final.
    /// Separate from the parent Final target, which survives child output.
    pub(super) streamed_text_idx: Option<usize>,
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
    pub(super) pending_attachment_view: crate::pending_attachments::PendingAttachmentView,
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
    pub(super) archive_browser: Option<crate::compaction_history::ArchiveBrowser>,
    pub(super) doc_view: Option<CidDocView>,
    pub(super) doc_history: Vec<(String, Option<usize>)>,
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
    /// Transient message shown in the browse and detail footers until this
    /// instant. Browse actions that cannot apply to the focused item set it,
    /// so `e`/`d`/`r` and friends never silently do nothing.
    pub(super) footer_notice: Option<FooterNotice>,
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
    /// Which surface owns keys while a `ConfirmInteraction` tray is pending.
    pub(crate) interaction_focus: InteractionFocus,
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

/// Keyboard focus while a `ConfirmInteraction` tray is pending. The tray owns
/// input unless an exclusive read view (browse/detail/cid document/subagent
/// view/archive) retains focus for reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum InteractionFocus {
    #[default]
    Tray,
    Reading,
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
    pub(super) fn is_terminal(&self) -> bool {
        matches!(
            self.snapshot.status,
            SubAgentProgressStatus::Done
                | SubAgentProgressStatus::Failed
                | SubAgentProgressStatus::Cancelled
        )
    }

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
    pub streamed_text_idx: Option<usize>,
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
            streamed_text_idx: None,
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

impl std::fmt::Debug for ConfirmToolUseState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfirmToolUse")
            .field("arguments", &self.arguments)
            .field("tool_name", &self.tool_name)
            .field("reason", &self.reason)
            .field("session_id", &self.session_id)
            .field("cluster", &self.cluster)
            .field("tool_call_id", &self.tool_call_id)
            .field("submission_id", &self.submission_id)
            .field("view", &self.view)
            .field("has_template", &self.has_template)
            .field("template_text", &self.template_text)
            .field("opened_at", &self.opened_at)
            .field("last_key_at", &self.last_key_at)
            .field("submitting", &self.submitting)
            .finish_non_exhaustive()
    }
}

impl Clone for ConfirmToolUseState {
    // qual:allow(dry, boilerplate) reason: "ScrollState has no Clone; preserve the existing manual copy of its position and follow state."
    fn clone(&self) -> Self {
        ConfirmToolUseState {
            arguments: self.arguments.clone(),
            tool_name: self.tool_name.clone(),
            reason: self.reason.clone(),
            session_id: self.session_id.clone(),
            cluster: self.cluster.clone(),
            tool_call_id: self.tool_call_id.clone(),
            submission_id: self.submission_id.clone(),
            confirmation_route: self.confirmation_route.clone(),
            submission_cancel: Arc::clone(&self.submission_cancel),
            submission_error: self.submission_error.clone(),
            view: self.view,
            has_template: self.has_template,
            template_text: self.template_text.clone(),
            // ScrollState doesn't implement Clone, so create a new one
            // preserving only the position and follow state
            scroll: {
                let mut new_scroll = ratatui_widget_scrolling::ScrollState::new();
                new_scroll.position = self.scroll.position;
                new_scroll.follow = self.scroll.follow;
                new_scroll.last_max_position = self.scroll.last_max_position;
                new_scroll
            },
            message: self.message.clone(),
            opened_at: self.opened_at,
            last_key_at: self.last_key_at,
            submitting: self.submitting,
        }
    }
}

/// Modal dialog state for destructive action confirmations.
/// Used by D5 for delete/rewind confirmations.
#[allow(dead_code)]
pub(super) enum ModalState {
    /// Confirmation for deleting one or more transcript entries.
    ConfirmDelete {
        from: usize,
        to: usize,
        target: TranscriptActionTarget,
    },
    /// Confirmation for rewinding session to a specific entry.
    ConfirmRewind {
        seq: usize,
        user_text: Option<String>,
        target: TranscriptActionTarget,
    },
    /// Confirmation for a tool call gated by tool confirmation or legacy approvals.
    /// The reply channel lives in `App::pending_confirm_reply`.
    ConfirmToolUse(Box<ConfirmToolUseState>),
    /// Durable middleware request; never answered through the legacy bool route.
    ConfirmInteraction(Box<crate::interaction_confirmation::InteractionModal>),
    /// Non-modal A2UI surface action form (plan T5.1). User-initiated only;
    /// never auto-presented, and closing it cancels nothing.
    SurfaceAction(Box<crate::surfaces::SurfaceActionModal>),
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
            Self::ConfirmDelete { from, to, target } => f
                .debug_struct("ConfirmDelete")
                .field("from", from)
                .field("to", to)
                .field("target", target)
                .finish(),
            Self::ConfirmRewind {
                seq,
                user_text,
                target,
            } => f
                .debug_struct("ConfirmRewind")
                .field("seq", seq)
                .field("user_text", user_text)
                .field("target", target)
                .finish(),
            Self::ConfirmInteraction(state) => {
                f.debug_tuple("ConfirmInteraction").field(state).finish()
            }
            Self::SurfaceAction(state) => f
                .debug_tuple("SurfaceAction")
                .field(&state.snapshot.surface_id)
                .finish(),
            Self::ConfirmToolUse(state) => std::fmt::Debug::fmt(state, f),
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
            Self::ConfirmDelete { from, to, target } => Self::ConfirmDelete {
                from: *from,
                to: *to,
                target: target.clone(),
            },
            Self::ConfirmRewind {
                seq,
                user_text,
                target,
            } => Self::ConfirmRewind {
                seq: *seq,
                user_text: user_text.clone(),
                target: target.clone(),
            },
            Self::ConfirmInteraction(state) => Self::ConfirmInteraction(state.clone()),
            Self::SurfaceAction(state) => Self::SurfaceAction(state.clone()),
            Self::ConfirmToolUse(state) => Self::ConfirmToolUse(state.clone()),

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

fn same_tool_confirmation(a: &ConfirmToolUseState, b: &ConfirmToolUseState) -> bool {
    // Scroll and message are intentionally excluded, matching the legacy comparison.
    (
        &a.arguments,
        &a.tool_name,
        &a.reason,
        &a.session_id,
        &a.cluster,
        &a.tool_call_id,
        &a.submission_id,
        &a.submission_error,
    ) == (
        &b.arguments,
        &b.tool_name,
        &b.reason,
        &b.session_id,
        &b.cluster,
        &b.tool_call_id,
        &b.submission_id,
        &b.submission_error,
    ) && (
        a.view,
        a.has_template,
        &a.template_text,
        a.opened_at,
        a.last_key_at,
        a.submitting,
    ) == (
        b.view,
        b.has_template,
        &b.template_text,
        b.opened_at,
        b.last_key_at,
        b.submitting,
    )
}

impl PartialEq for ModalState {
    // qual:allow(complexity, max_function_lines=76) reason: "the eq match grows one arm per modal variant"
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ConfirmDelete {
                    from: a_from,
                    to: a_to,
                    target: a_target,
                },
                Self::ConfirmDelete {
                    from: b_from,
                    to: b_to,
                    target: b_target,
                },
            ) => (a_from, a_to, a_target) == (b_from, b_to, b_target),
            (
                Self::ConfirmRewind {
                    seq: a_seq,
                    user_text: a_user_text,
                    target: a_target,
                },
                Self::ConfirmRewind {
                    seq: b_seq,
                    user_text: b_user_text,
                    target: b_target,
                },
            ) => (a_seq, a_user_text, a_target) == (b_seq, b_user_text, b_target),
            (Self::ConfirmInteraction(a), Self::ConfirmInteraction(b)) => a.same_request(b),
            (Self::SurfaceAction(a), Self::SurfaceAction(b)) => a.same_form(b),
            (Self::ConfirmToolUse(a), Self::ConfirmToolUse(b)) => same_tool_confirmation(a, b),
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
            Self::ConfirmDelete { from, to, .. } if from == to => {
                Some(format!("Delete entry {from}? [y/N]"))
            }
            Self::ConfirmDelete { from, to, .. } => {
                Some(format!("Delete entries {from}–{to}? [y/N]"))
            }
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
    /// Runtime-authored context note (timing notice, sub-agent start, turn
    /// interruption, compaction summary) injected with the user role.
    /// `compact` is the dim one-liner shown in the transcript; `text` keeps
    /// the full raw note for the detail view.
    RuntimeNote {
        text: String,
        compact: String,
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
        /// Identity of the subagent reply block; ordinary tool results have no owner.
        subagent_reply_owner: Option<(MonitoredSessionKey, Option<String>)>,
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
        /// Live concise activity title updated in place during tool execution.
        title: Option<String>,
        /// Non-terminal or terminal tool execution status.
        status: Option<ToolStatus>,
        /// Dynamic tool categorization kind.
        kind: Option<ToolKind>,
        /// Affected file or workspace locations updated during execution.
        locations: Vec<ToolLocation>,
        /// Live token usage snapshot reported by the tool during execution.
        usage: Option<CompletionTokenUsage>,
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
    /// A selectable link extracted from a markdown block.
    MarkdownLink {
        text: String,
        url: String,
    },
    /// A published non-modal A2UI surface from a completed tool result
    /// (plan T5.1). Rendered inline; Enter on a focused row opens the action
    /// form. `resolved` is the accepted action name once a submission was
    /// durably accepted (first-wins), refreshed from the session log.
    UserSurface {
        surface_id: String,
        payload: serde_json::Value,
        resolved: Option<String>,
    },
    /// Static navigable handoff row produced from the `handoff` marker in a
    /// handoff tool result. Links to the child session without live status/timer.
    /// Distinct from SubAgentSession (which carries status, metrics, cancellation).
    HandoffSession {
        agent: String,
        session_id: String,
    },
}

impl TranscriptItem {
    /// Apply an in-place tool update patch to this item if it is an active `ToolCall`.
    /// Returns `true` if the patch was applied, or `false` if this item is not a `ToolCall`
    /// or has already reached a terminal state (Completed/Failed).
    pub(crate) fn apply_tool_update(
        &mut self,
        markdown: Option<String>,
        status: Option<ToolStatus>,
        title: Option<String>,
        kind: Option<ToolKind>,
        locations: Option<Vec<ToolLocation>>,
        usage: Option<CompletionTokenUsage>,
    ) -> bool {
        if let TranscriptItem::ToolCall {
            body,
            rendered_cache,
            title: item_title,
            status: item_status,
            kind: item_kind,
            locations: item_locations,
            usage: item_usage,
            final_elapsed_ms,
            ..
        } = self
        {
            if final_elapsed_ms.is_some() {
                // Reject updates after completion/cancellation (D3)
                return false;
            }
            if let Some(md) = markdown {
                if !md.trim().is_empty() {
                    *body = Some(ToolCallBody::Markdown(md));
                }
            }
            if let Some(t) = title {
                *item_title = Some(t);
            }
            if let Some(s) = status {
                if !matches!(s, ToolStatus::Completed | ToolStatus::Failed) {
                    *item_status = Some(s);
                }
            }
            if let Some(k) = kind {
                *item_kind = Some(k);
            }
            if let Some(locs) = locations {
                *item_locations = locs;
            }
            if let Some(u) = usage {
                *item_usage = Some(u);
            }
            *rendered_cache = None;
            true
        } else {
            false
        }
    }

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
            TranscriptItem::RuntimeNote { seq, .. } => *seq,
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
                | TranscriptItem::RuntimeNote { .. }
                | TranscriptItem::AssistantText { .. }
                | TranscriptItem::ToolCall { .. }
                | TranscriptItem::CompactionMarker { .. }
                | TranscriptItem::SubAgentSession { .. }
                | TranscriptItem::MarkdownLink { .. }
                | TranscriptItem::HandoffSession { .. }
                | TranscriptItem::UserSurface { .. }
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
            streamed_text_idx: None,
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
            pending_attachment_view: Default::default(),
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
            archive_browser: None,
            doc_view: None,
            doc_history: Vec::new(),
            detail_view_text: None,
            detail_view_entry: None,
            detail_view_title: None,
            transcript_browsing: false,
            browsing_view_scroll: ratatui_widget_scrolling::ScrollState::new(),
            footer_notice: None,
            scroll_to_focused_item: false,
            use_utc_timestamps: false,
            monitored_sessions: HashMap::new(),
            subagent_view_stack: Vec::new(),
            interaction_focus: InteractionFocus::Tray,
        }
    }
}

/// Transient footer message with an expiry, shown by the browse and detail
/// footers in place of the shortcut hints.
pub(super) struct FooterNotice {
    pub(super) text: String,
    pub(super) until: std::time::Instant,
}

/// How long a footer notice stays visible.
const FOOTER_NOTICE_DURATION: std::time::Duration = std::time::Duration::from_secs(3);

impl App {
    /// Returns the (min, max) indices of the current transcript selection.
    /// Used by render_detail_view to determine which entries to display.
    pub(super) fn selected_transcript_range(&self) -> (usize, usize) {
        let f = self.transcript_focus.unwrap_or(0);
        let a = self.transcript_selection_anchor.unwrap_or(f);
        (f.min(a), f.max(a))
    }

    /// Show a transient notice in the browse/detail footer, e.g. when a
    /// browse action cannot apply to the focused item.
    pub(super) fn show_footer_notice(&mut self, text: &str) {
        self.footer_notice = Some(FooterNotice {
            text: text.to_string(),
            until: std::time::Instant::now() + FOOTER_NOTICE_DURATION,
        });
    }

    /// The active footer notice, or `None` once it has expired.
    pub(super) fn footer_notice_text(&self) -> Option<&str> {
        self.footer_notice
            .as_ref()
            .filter(|notice| std::time::Instant::now() < notice.until)
            .map(|notice| notice.text.as_str())
    }
}

pub(crate) enum ToolConfirmationEvent {
    /// A worker requested tool confirmation via NATS. The
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

pub(crate) type AdoptedSessionSink = (
    String,
    std::sync::Arc<dyn harnx_runtime::config::session::SessionAppendSink>,
);

pub(crate) enum TuiEvent {
    /// Durable non-modal surface snapshot / action outcome (plan T5.1).
    Surface(crate::surfaces::SurfaceEvent),
    OperatorToolFinished {
        task: AbortSignal,
        output: String,
        error: Option<String>,
    },
    /// Mid-run output from a `.credentials` command (connect link shown while
    /// the flow is still waiting in the browser).
    CredentialsUpdate {
        task: AbortSignal,
        text: String,
    },
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
    /// Shared session stream successfully attached (including idle attach
    /// and same-session reconnect).
    SessionStreamAttached {
        stamp: crate::event_isolation::EventStamp,
        session_id: String,
        cluster: String,
        active: bool,
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
    Interaction(crate::interaction_confirmation::InteractionEvent),
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
    /// A background transcript rebuild/repair/refresh finished loading.
    /// Fenced on seq, target, attachment generation, refresh revision, prompt
    /// state, and live mutation fence before mutating the transcript.
    HistoryRebuildFinished(Box<HistoryRebuildCompletion>),
    /// Paced retry timer for pending initial-history repair fired.
    HistoryRepairRetryDue {
        target: HistoryTarget,
        attachment_gen: u64,
        refresh_rev: u64,
    },
    /// A backgrounded history command finished. History commands (session
    /// switches, transcript mutations) can wait on managed-local
    /// acquisition or the session log, so they run off the input loop and
    /// land here; `finish_command` then applies their output as usual.
    HistoryCommandFinished {
        line: String,
        prev_session: Option<String>,
        prev_agent: Option<String>,
        output: String,
        result: Result<harnx_runtime::commands::CommandOutcome, anyhow::Error>,
        adopted_sink: Option<AdoptedSessionSink>,
    },
    /// A backgrounded `.set title` rename finished against its submission-time
    /// captured destination. Applied only while `seq`, target, and storage key
    /// still match.
    TitleRenameFinished {
        seq: u64,
        target: HistoryTarget,
        storage_key: Option<String>,
        title: String,
        result: Result<(), anyhow::Error>,
    },
    /// A backgrounded session-picker refresh finished. Session listing can
    /// wait on managed-local acquisition, so it runs off the input loop and
    /// lands here; it applies only while the originating picker incarnation
    /// and target are still active and the request seq has not been superseded
    /// or cancelled.
    PickerSessionsLoaded {
        seq: u64,
        picker_incarnation: u64,
        target: PickerTarget,
        sessions: Vec<harnx_runtime::config::SessionMeta>,
        fetch_error: Option<String>,
    },
    /// A backgrounded "New session" reservation from the session picker
    /// finished; applying it switches the session and reconciles. `seq`
    /// fences stale completions against a newer reservation.
    NewSessionReserved {
        seq: u64,
        session_id: String,
        origin_agent: Option<String>,
        origin_session: Option<String>,
    },
    /// A backgrounded "New session" reservation failed; the picker the
    /// user started it from is restored with the error. `seq` fences
    /// stale completions against a newer reservation.
    NewSessionReservationFailed {
        seq: u64,
        error: String,
        sessions: Vec<harnx_runtime::config::SessionMeta>,
        selected: usize,
        origin_agent: Option<String>,
        origin_session: Option<String>,
    },
    /// A backgrounded picker unread toggle (`u`/`U`) finished. The picker
    /// re-lists only if it is still the current view; `seq` fences stale
    /// completions.
    PickerUnreadToggleFinished {
        seq: u64,
    },
    /// A backgrounded info-overlay load (`.info session` / `.dump session`)
    /// finished; the overlay opens only if `seq`, `target`, and `storage_key`
    /// still match the active request (not cancelled, superseded, or navigated).
    InfoOverlayLoaded {
        seq: u64,
        target: HistoryTarget,
        storage_key: Option<String>,
        display_text: String,
        info_type: crate::input_commands::InfoOverlayType,
    },
}
