//! Background history work for the TUI input loop: the cancellable transcript
//! rebuild/repair/refresh pipeline and the serialized queue of history-affecting
//! dot commands (designs 6, 9, and 10). None of it blocks the input/event loop;
//! completions arrive as events and share one serialized ordering.
//!
//! The same rule covers the other broker-waiting foreground actions that
//! share this module's shape: session-picker refreshes, the picker's "New
//! session" reservation, picker unread toggles, and `.info session` /
//! `.dump session` overlay loads. Each runs as cancellable background work
//! with its own abort signal, so a recovering broker cannot freeze the TUI,
//! and Ctrl-C cancels the caller work without poisoning later work: Enter's
//! `abort_signal.reset()` only resets the shared prompt signal, never a
//! per-job signal.

use crate::event_isolation::EventStamp;
use crate::strip_ansi;
use crate::types::{
    AdoptedSessionSink, HistoryDestination, HistoryRebuildCompletion, HistoryTarget, PickerTarget,
    QueuedHistoryCommand, SessionCommandTarget, TranscriptActionTarget, TranscriptItem, Tui,
    TuiEvent,
};
use anyhow::Result;
use crossterm::ExecutableCommand;
use harnx_core::abort::wait_abort_signal;
use harnx_render::pretty_error_string;
use harnx_runtime::utils::{create_abort_signal, AbortSignal};
use std::time::Duration;

pub(crate) const INITIAL_HISTORY_REPAIR_DELAY: Duration = Duration::from_secs(1);
const MAX_HISTORY_REPAIR_DELAY: Duration = Duration::from_secs(30);
const LOADING_HISTORY_STATUS: &str = "Loading session history…";
const HISTORY_LOAD_ERROR_PREFIX: &str = "Failed to load session history: ";
const STALE_HISTORY_ERROR_PREFIX: &str =
    "Transcript may be stale; failed to reload session history: ";

/// Command output is rendered into the transcript without ANSI decoration
/// and without the trailing newline the raw buffer carries.
pub(crate) fn history_command_display_output(output: &str) -> String {
    strip_ansi(output).trim_end_matches('\n').to_string()
}

/// Commands that rewrite the transcript; `finish_command` rebuilds the view
/// after them instead of resuming at the old position.
pub(crate) fn is_mutation_command(line: &str) -> bool {
    let Some((command, args)) = line.trim_start().split_once(char::is_whitespace) else {
        return false;
    };
    command == ".rewind"
        || (matches!(command, ".edit" | ".delete") && args.trim_start().starts_with("message "))
}

/// Commands that replace or reset the current session's log coordinates on
/// the same target, invalidating prior row coordinates and confirmations.
pub(crate) fn invalidates_transcript_coordinates(line: &str) -> bool {
    let (command, rest) = line
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((line, ""));
    is_mutation_command(line)
        || matches!(
            (command, rest.trim()),
            (".empty" | ".compact" | ".reset", "session") | (".reset", "repl")
        )
}

/// Session-wide history commands that must stay bound to the submitting
/// session target without requiring transcript row bindings.
pub(crate) fn is_session_bound_history_command(line: &str) -> bool {
    if is_mutation_command(line) {
        return false;
    }
    if invalidates_transcript_coordinates(line) {
        return true;
    }
    let (command, rest) = line
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((line, ""));
    command == ".edit" && rest.trim() == "session"
}

#[derive(Clone, Copy)]
enum SessionClearKind {
    Empty,
    Reset,
}

fn session_clear_kind(line: &str) -> Option<SessionClearKind> {
    let (command, rest) = line
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((line, ""));
    match (command, rest.trim()) {
        (".empty", "session") => Some(SessionClearKind::Empty),
        (".reset", "session" | "repl") => Some(SessionClearKind::Reset),
        _ => None,
    }
}

fn apply_live_session_clear(
    config: &harnx_runtime::config::GlobalConfig,
    kind: SessionClearKind,
) -> Result<()> {
    let mut cfg = config.write();
    match kind {
        SessionClearKind::Empty => cfg.empty_session_after_persisted_clear(),
        SessionClearKind::Reset => cfg.reset_session_after_persisted_clear(),
    }
}

fn config_matches_session_target(
    config: &harnx_runtime::config::GlobalConfig,
    expected: &SessionCommandTarget,
) -> bool {
    let cfg = config.read();
    let target = history_target(&cfg);
    let storage_key = cfg.session.as_ref().map(|s| s.storage_key());
    target == expected.target && storage_key == expected.storage_key
}

fn config_matches_action_target(
    config: &harnx_runtime::config::GlobalConfig,
    expected: &TranscriptActionTarget,
) -> bool {
    let cfg = config.read();
    let cluster = cfg
        .remote_agent
        .as_ref()
        .map(|(_, cluster)| cluster.clone())
        .unwrap_or_else(|| cfg.default_cluster_key().to_string());
    let storage_key = cfg.session.as_ref().map(|s| s.storage_key());
    let session_id = cfg.session.as_ref().map(|s| s.id().to_string());
    let agent = cfg.active_agent_ref();
    cluster == expected.cluster
        && storage_key == expected.storage_key
        && session_id == expected.target.session_id
        && agent == expected.target.agent
        && cluster == expected.target.cluster
}

fn history_target(cfg: &harnx_runtime::config::Config) -> HistoryTarget {
    let cluster = cfg
        .remote_agent
        .as_ref()
        .map(|(_, cluster)| cluster.clone())
        .unwrap_or_else(|| cfg.default_cluster_key().to_string());
    HistoryTarget {
        session_id: cfg.session.as_ref().map(|s| s.id().to_string()),
        agent: cfg.active_agent_ref(),
        cluster,
    }
}

/// How a finished rebuild treats rows appended after it was scheduled and
/// which fences govern its completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RebuildMode {
    /// Keep rows appended after scheduling (command output, live stream
    /// chunks) after the rebuilt view — used for session/agent switches and
    /// mutation dot commands.
    PreserveTail,
    /// Replace the whole transcript with the loaded view — used for
    /// compaction completion and handoff navigation.
    ReplaceAll,
    /// Silent background repair of a failed initial history load (triggered on
    /// successful stream attachment or paced retry). Rebuilds the full
    /// initial transcript view and rejects stale attachments or live edits.
    RepairInitial,
    /// Silent turn-triggered shared-session refresh (`Turn::Started`,
    /// `Turn::Ended`, `Turn::Interrupted`, `SessionActivity(false)`).
    SharedRefresh,
}

/// Per-rebuild failure policy encoding how a failed load affects displayed
/// rows, error presentation, actionability gating, and paced background repair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RebuildFailurePolicy {
    /// Target switch (`PreserveTail`): replace view with initial error
    /// transcript (keeping post-switch tail), keep actionability retired, and
    /// arm paced repair.
    ReplaceWithInitialErrorAndRepair,
    /// Handoff (`ReplaceAll` after clearing transcript): surface error on
    /// cleared/live view (never leave blank), keep actionability retired, and
    /// arm paced repair.
    ShowHandoffErrorAndRepair,
    /// Same-target post-mutation or compaction reload: preserve displayed
    /// rows, append/update a stale-reload error notice, keep destructive row
    /// actions gated (`active_binding = None`), and arm paced repair.
    PreserveStaleWithErrorAndRepair,
    /// Paced initial repair or silent shared-session refresh: preserve
    /// displayed view; keep repair pending with paced backoff if repair is
    /// already active or mode is `RepairInitial`.
    KeepRepairOrPreserveSilently,
}

struct RebuildTaskMeta {
    seq: u64,
    target: HistoryTarget,
    attachment_gen: u64,
    refresh_rev: u64,
    mutation_fence: u64,
    live_stamp: Option<EventStamp>,
    preserved_from: usize,
    mode: RebuildMode,
    uses_initial_wrapper: bool,
    failure_policy: RebuildFailurePolicy,
}

impl RebuildMode {
    fn uses_initial_wrapper(self) -> bool {
        matches!(self, Self::PreserveTail | Self::RepairInitial)
    }

    fn is_shared_attachment_scoped(self) -> bool {
        matches!(self, Self::RepairInitial | Self::SharedRefresh)
    }

    fn shows_loading_line(self) -> bool {
        matches!(self, Self::PreserveTail)
    }
}

// qual:allow(coupling, oi) reason: "TUI behavior is split by concern, as in input_commands.rs and interaction_confirmation.rs; history work is its own concern."
impl Tui {
    /// Drain background history-command and rebuild completions so tests can
    /// observe the post-command transcript deterministically. Panics when a
    /// completion does not arrive in time — a wait-for-X timeout is a test
    /// failure, not a skip.
    #[cfg(test)]
    pub(crate) async fn flush_history_work_for_test(&mut self) {
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
        const POLL_SLEEP: std::time::Duration = std::time::Duration::from_millis(5);
        let started = std::time::Instant::now();
        loop {
            while let Ok(event) = self.event_rx.try_recv() {
                if let Err(error) = self.handle_tui_event(event).await {
                    panic!("history work event failed: {error:#}");
                }
            }
            if !self.history_command_running
                && self.history_command_queue.is_empty()
                && self.background_history_jobs_idle()
            {
                return;
            }
            if started.elapsed() >= BUDGET {
                panic!("background history work did not finish within {BUDGET:?}");
            }
            tokio::time::sleep(POLL_SLEEP).await;
        }
    }

    /// True when no background job of this module is in flight. Extracted so
    /// the polling helper below stays under the complexity budget. Only the
    /// test drain loop polls it.
    #[cfg(test)]
    fn background_history_jobs_idle(&self) -> bool {
        self.history_rebuild_abort.is_none()
            && self.title_rename_abort.is_none()
            && self.info_overlay_abort.is_none()
            && self.picker_refresh_abort.is_none()
            && self.new_session_reservation_abort.is_none()
            && self.picker_unread_toggle_abort.is_none()
    }

    pub(crate) fn current_history_target(&self) -> HistoryTarget {
        history_target(&self.config.read())
    }

    pub(crate) fn current_picker_target(&self) -> PickerTarget {
        let cfg = self.config.read();
        let cluster = cfg
            .remote_agent
            .as_ref()
            .map(|(_, cluster)| cluster.clone())
            .unwrap_or_else(|| cfg.default_cluster_key().to_string());
        let agent = cfg
            .remote_agent
            .as_ref()
            .map(|(agent, _)| agent.clone())
            .or_else(|| cfg.agent.as_ref().map(|agent| agent.name().to_string()));
        PickerTarget { cluster, agent }
    }

    pub(crate) fn current_session_storage_key(&self) -> Option<String> {
        self.config
            .read()
            .session
            .as_ref()
            .map(|session| session.storage_key())
    }

    /// Capture both the active [`HistoryTarget`] and canonical session storage
    /// key under one brief read guard so callers never nest `config.read()`
    /// behind a waiting writer.
    pub(crate) fn current_history_target_and_storage_key(&self) -> (HistoryTarget, Option<String>) {
        let cfg = self.config.read();
        (
            history_target(&cfg),
            cfg.session.as_ref().map(|session| session.storage_key()),
        )
    }

    pub(crate) fn current_history_destination(&self) -> HistoryDestination {
        let (target, storage_key) = self.current_history_target_and_storage_key();
        HistoryDestination {
            target,
            storage_key,
        }
    }

    pub(crate) fn is_unsettled_write_blocking_current_target(&self) -> bool {
        self.transcript_view_ownership
            .unsettled_write_destination
            .as_ref()
            .is_some_and(|dest| dest == &self.current_history_destination())
    }

    pub(crate) fn current_transcript_action_target(&self) -> TranscriptActionTarget {
        let (cluster, storage_key, session_id, agent) = {
            let cfg = self.config.read();
            let cluster = cfg
                .remote_agent
                .as_ref()
                .map(|(_, cluster)| cluster.clone())
                .unwrap_or_else(|| cfg.default_cluster_key().to_string());
            let storage_key = cfg.session.as_ref().map(|s| s.storage_key());
            let session_id = cfg.session.as_ref().map(|s| s.id().to_string());
            let agent = cfg.active_agent_ref();
            (cluster, storage_key, session_id, agent)
        };
        TranscriptActionTarget {
            cluster: cluster.clone(),
            storage_key,
            target: HistoryTarget {
                session_id,
                agent,
                cluster,
            },
            view_revision: self.transcript_view_ownership.view_revision,
        }
    }

    pub(crate) fn current_session_command_target(&self) -> SessionCommandTarget {
        let (target, storage_key) = self.current_history_target_and_storage_key();
        SessionCommandTarget {
            target,
            storage_key,
            target_epoch: self.transcript_view_ownership.session_fence.target_epoch,
        }
    }

    pub(crate) fn actionable_transcript_binding(&self) -> Option<TranscriptActionTarget> {
        let binding = self.transcript_view_ownership.active_binding.as_ref()?;
        let current = self.current_transcript_action_target();
        (binding == &current).then(|| binding.clone())
    }

    pub(crate) fn is_title_rename_busy(&self) -> bool {
        self.title_rename_abort.is_some() || self.history_command_running
    }

    pub(crate) fn show_title_rename_busy_notice(&mut self) {
        let message = if self.title_rename_abort.is_some() {
            "Session rename already in progress — wait or press Ctrl+C"
        } else {
            "Session command in progress — wait or press Ctrl+C before renaming"
        };
        self.app.show_footer_notice(message);
    }

    pub(crate) fn retire_pending_title_rename(&mut self) {
        retire_abort_slot(&mut self.title_rename_abort, &mut self.title_rename_seq);
    }

    pub(crate) fn validate_history_command_target(
        &self,
        expected: Option<&TranscriptActionTarget>,
    ) -> bool {
        self.validate_history_command_targets(expected, None)
    }

    pub(crate) fn validate_history_command_targets(
        &self,
        expected_row: Option<&TranscriptActionTarget>,
        expected_session: Option<&SessionCommandTarget>,
    ) -> bool {
        let row_ok = match expected_row {
            Some(bound) => self
                .actionable_transcript_binding()
                .is_some_and(|current| &current == bound),
            None => true,
        };
        let session_ok = match expected_session {
            Some(expected) => &self.current_session_command_target() == expected,
            None => true,
        };
        row_ok && session_ok
    }

    fn clear_transcript_overlays(&mut self) {
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.app.transcript_browsing = false;
        self.reset_pending_attachment_view();
        self.app.detail_view_open = false;
        self.app.detail_view_entry = None;
        self.app.detail_view_text = None;
        self.app.detail_view_title = None;
        self.app.doc_view = None;
        self.app.doc_history.clear();
        self.app.archive_browser = None;
    }

    /// Retire prior-target rows, focus, selection, detail/document/archive
    /// overlays, and transcript-derived confirmations on a real target switch.
    pub(crate) fn retire_prior_target_transcript_actions(&mut self) {
        self.retire_pending_title_rename();
        let fence = &mut self.transcript_view_ownership.session_fence;
        fence.target_epoch = fence.target_epoch.saturating_add(1);
        if fence.running_bound_target.is_some() {
            if let Some(abort) = self.history_command_abort.take() {
                abort.set_ctrlc();
            }
        }
        self.transcript_view_ownership.view_revision = self
            .transcript_view_ownership
            .view_revision
            .saturating_add(1);
        self.transcript_view_ownership.active_binding = None;
        self.app.scroll_state.follow = true;
        self.clear_transcript_overlays();
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.streamed_text_idx = None;
        self.app.last_ui_output_source = None;
        self.app.pending_thought_text.clear();
        self.app.pending_thought_source = None;
        self.app.pending_tool_seq = None;
        self.retire_transcript_confirmation_modal();
        self.reject_stale_queued_history_commands();
        self.retire_info_overlay();
        if !matches!(
            self.app.modal,
            Some(crate::types::ModalState::SessionPicker { .. })
        ) || self.picker_refresh_target.as_ref() != Some(&self.current_picker_target())
        {
            self.retire_picker_refresh();
        }
    }

    pub(crate) fn retire_info_overlay(&mut self) {
        retire_abort_slot(&mut self.info_overlay_abort, &mut self.info_overlay_seq);
    }

    pub(crate) fn retire_picker_refresh(&mut self) {
        retire_abort_slot(&mut self.picker_refresh_abort, &mut self.picker_refresh_seq);
        self.picker_refresh_target = None;
        self.picker_incarnation = self.picker_incarnation.saturating_add(1);
    }

    pub(crate) fn cancel_inflight_picker_unread_toggle(&mut self) {
        retire_abort_slot(
            &mut self.picker_unread_toggle_abort,
            &mut self.picker_unread_toggle_seq,
        );
    }

    /// Invalidate row actionability when a same-target mutation or session
    /// reset begins so queued/open row actions from pre-mutation coordinates
    /// cannot fire until the rebuilt view commits.
    pub(crate) fn retire_transcript_actions_for_pending_mutation(&mut self) {
        self.transcript_view_ownership.view_revision = self
            .transcript_view_ownership
            .view_revision
            .saturating_add(1);
        self.transcript_view_ownership.active_binding = None;
        self.retire_transcript_confirmation_modal();
        self.reject_stale_queued_history_commands();
    }

    fn retire_transcript_confirmation_modal(&mut self) {
        if matches!(
            self.app.modal,
            Some(
                crate::types::ModalState::ConfirmDelete { .. }
                    | crate::types::ModalState::ConfirmRewind { .. }
            )
        ) {
            self.app.modal = None;
        }
    }

    pub(crate) fn reject_stale_queued_history_commands(&mut self) {
        let mut kept = std::collections::VecDeque::with_capacity(self.history_command_queue.len());
        let mut dropped = false;
        while let Some(cmd) = self.history_command_queue.pop_front() {
            if self.validate_history_command_targets(
                cmd.expected_target.as_ref(),
                cmd.expected_session.as_ref(),
            ) {
                kept.push_back(cmd);
            } else {
                dropped = true;
            }
        }
        self.history_command_queue = kept;
        if dropped {
            self.show_stale_transcript_notice();
        }
    }

    pub(crate) fn is_history_repair_pending(&self) -> bool {
        self.history_repair_target.as_ref() == Some(&self.current_history_target())
    }

    pub(crate) fn note_live_transcript_mutation(&mut self) {
        self.transcript_mutation_seq = self.transcript_mutation_seq.saturating_add(1);
    }

    pub(crate) fn abort_history_repair_retry(&mut self) {
        if let Some(handle) = self.history_repair_retry_handle.take() {
            handle.abort();
        }
    }

    pub(crate) fn cancel_inflight_history_rebuild(&mut self) {
        if let Some(previous) = self.history_rebuild_abort.take() {
            previous.set_ctrlc();
        }
        if let Some(handle) = self.history_rebuild_handle.take() {
            handle.abort();
        }
    }

    /// Cancel every in-flight background job this module owns (rebuild,
    /// history command, info overlay, picker refresh, new-session
    /// reservation, picker unread toggle) and drop queued history commands.
    /// Called on Ctrl-C: caller work stops, the TUI stays alive, and later
    /// work starts with fresh signals so nothing is left un-aborted or poisoned.
    pub(crate) fn cancel_background_history_work(&mut self) {
        if let Some(abort) = self.history_rebuild_abort.take() {
            abort.set_ctrlc();
            // Bump the fence so a completion queued just before Ctrl-C is
            // discarded too.
            self.history_rebuild_seq = self.history_rebuild_seq.saturating_add(1);
        }
        if let Some(handle) = self.history_rebuild_handle.take() {
            handle.abort();
        }
        self.abort_history_repair_retry();
        if let Some(abort) = self.history_command_abort.take() {
            abort.set_ctrlc();
        }
        self.history_command_queue.clear();
        if self.title_rename_abort.is_some() {
            self.retire_pending_title_rename();
            self.app.transcript.push(TranscriptItem::ErrorText(
                "session title write cancelled".to_string(),
            ));
            self.note_live_transcript_mutation();
        }
        retire_abort_slot(&mut self.info_overlay_abort, &mut self.info_overlay_seq);
        retire_abort_slot(&mut self.picker_refresh_abort, &mut self.picker_refresh_seq);
        self.picker_refresh_target = None;
        retire_abort_slot(
            &mut self.new_session_reservation_abort,
            &mut self.new_session_reservation_seq,
        );
        retire_abort_slot(
            &mut self.picker_unread_toggle_abort,
            &mut self.picker_unread_toggle_seq,
        );
        self.schedule_history_repair_retry();
    }

    /// Schedule the transcript rebuild as cancellable background work so a
    /// slow managed-local acquisition or history load never blocks the
    /// input/event loop (design 9).
    #[cfg(test)]
    pub(crate) fn schedule_history_rebuild_with_mode(&mut self, mode: RebuildMode) {
        self.schedule_history_rebuild_inner(
            mode,
            mode.uses_initial_wrapper(),
            RebuildFailurePolicy::KeepRepairOrPreserveSilently,
            None,
        );
    }

    pub(crate) fn schedule_handoff_history_rebuild(&mut self) {
        self.transcript_view_ownership.unresolved_navigation_target = None;
        self.schedule_history_rebuild_inner(
            RebuildMode::ReplaceAll,
            false,
            RebuildFailurePolicy::ShowHandoffErrorAndRepair,
            None,
        );
    }

    pub(crate) fn schedule_compaction_history_rebuild(&mut self) {
        self.schedule_history_rebuild_inner(
            RebuildMode::ReplaceAll,
            false,
            RebuildFailurePolicy::PreserveStaleWithErrorAndRepair,
            None,
        );
    }

    #[cfg(test)]
    pub(crate) fn schedule_history_rebuild(&mut self) {
        self.schedule_history_rebuild_inner(
            RebuildMode::PreserveTail,
            true,
            RebuildFailurePolicy::KeepRepairOrPreserveSilently,
            None,
        );
    }

    fn has_unresolved_navigation_to_current_destination(&self) -> bool {
        let current_dest = self.current_history_destination();
        self.transcript_view_ownership
            .unresolved_navigation_target
            .as_ref()
            == Some(&current_dest)
    }

    /// A rebuild aimed at a destination with an unresolved navigation keeps
    /// replacement-on-error semantics even when this read was scheduled for
    /// another reason; otherwise a superseding failed read could preserve the
    /// previous destination's stale rows under the current header (#2922).
    fn resolve_rebuild_failure_policy(
        &self,
        failure_policy: RebuildFailurePolicy,
    ) -> RebuildFailurePolicy {
        if failure_policy != RebuildFailurePolicy::ShowHandoffErrorAndRepair
            && self.has_unresolved_navigation_to_current_destination()
        {
            RebuildFailurePolicy::ReplaceWithInitialErrorAndRepair
        } else {
            failure_policy
        }
    }

    fn clear_unresolved_navigation_for_current_destination(&mut self) {
        if self.has_unresolved_navigation_to_current_destination() {
            self.transcript_view_ownership.unresolved_navigation_target = None;
        }
    }

    pub(crate) fn schedule_history_rebuild_for_command(&mut self, target_switched: bool) {
        if target_switched {
            self.transcript_view_ownership.unresolved_navigation_target =
                Some(self.current_history_destination());
        }
        let failure_policy = if self.has_unresolved_navigation_to_current_destination() {
            RebuildFailurePolicy::ReplaceWithInitialErrorAndRepair
        } else {
            RebuildFailurePolicy::PreserveStaleWithErrorAndRepair
        };
        self.schedule_history_rebuild_inner(RebuildMode::PreserveTail, true, failure_policy, None);
    }

    pub(crate) fn schedule_history_repair(&mut self) {
        let uses_wrapper = self.history_repair_uses_wrapper;
        self.schedule_history_rebuild_inner(
            RebuildMode::RepairInitial,
            uses_wrapper,
            RebuildFailurePolicy::KeepRepairOrPreserveSilently,
            None,
        );
    }

    pub(crate) fn schedule_shared_session_refresh(&mut self, stamp: &EventStamp) {
        if !stamp.allows(&self.live_events) || self.current_prompt_abort.is_some() {
            return;
        }
        self.schedule_history_rebuild_inner(
            RebuildMode::SharedRefresh,
            false,
            RebuildFailurePolicy::KeepRepairOrPreserveSilently,
            Some(stamp.clone()),
        );
    }

    fn schedule_history_rebuild_inner(
        &mut self,
        mode: RebuildMode,
        uses_initial_wrapper: bool,
        failure_policy: RebuildFailurePolicy,
        live_stamp: Option<EventStamp>,
    ) {
        let failure_policy = self.resolve_rebuild_failure_policy(failure_policy);
        // At most one snapshot load in flight: cancel and abort any previous one.
        self.cancel_inflight_history_rebuild();
        self.abort_history_repair_retry();
        self.history_rebuild_seq = self.history_rebuild_seq.saturating_add(1);
        self.history_refresh_rev = self.history_refresh_rev.saturating_add(1);
        let target = self.current_history_target();
        if self
            .history_repair_target
            .as_ref()
            .is_some_and(|pending| pending != &target)
        {
            self.history_repair_target = None;
            self.history_repair_uses_wrapper = true;
            self.history_repair_retry_delay = INITIAL_HISTORY_REPAIR_DELAY;
        }
        if self.is_unsettled_write_blocking_current_target() {
            return;
        }
        if mode.shows_loading_line() {
            self.app.transcript.push(TranscriptItem::StatusLine(
                LOADING_HISTORY_STATUS.to_string(),
            ));
        }
        let preserved_from = self.app.transcript.len();
        let live_stamp = live_stamp.or_else(|| {
            (mode.is_shared_attachment_scoped() || self.is_history_repair_pending())
                .then(|| EventStamp::snapshot(&self.live_events))
        });
        let abort = create_abort_signal();
        self.history_rebuild_abort = Some(abort.clone());
        let handle = self.spawn_history_rebuild_task(
            RebuildTaskMeta {
                seq: self.history_rebuild_seq,
                target,
                attachment_gen: self.session_attachment_gen,
                refresh_rev: self.history_refresh_rev,
                mutation_fence: self.transcript_mutation_seq,
                live_stamp,
                preserved_from,
                mode,
                uses_initial_wrapper,
                failure_policy,
            },
            abort,
        );
        self.history_rebuild_handle = Some(handle);
    }

    fn spawn_history_rebuild_task(
        &self,
        meta: RebuildTaskMeta,
        abort: harnx_runtime::utils::AbortSignal,
    ) -> tokio::task::JoinHandle<()> {
        let config = self.config.clone();
        let event_tx = self.event_tx.clone();
        #[cfg(test)]
        let load_override = self.history_load_override.clone();
        tokio::spawn(async move {
            // Read from the same selection we will fence the result against.
            let snapshot = config.read().clone();
            let snapshot_matches = history_target(&snapshot) == meta.target;
            let config = std::sync::Arc::new(harnx_runtime::config::ConfigLock::new(snapshot));
            let result = if snapshot_matches {
                tokio::select! {
                    biased;
                    _ = wait_abort_signal(&abort) => return,
                    result = load_snapshot_items(
                        &config,
                        meta.mode,
                        meta.uses_initial_wrapper,
                        &abort,
                        #[cfg(test)]
                        load_override,
                    ) => result,
                }
            } else {
                Err("Session changed before history read started".to_string())
            };
            if abort.aborted() {
                return;
            }
            let _ = event_tx.send(TuiEvent::HistoryRebuildFinished(Box::new(
                HistoryRebuildCompletion {
                    seq: meta.seq,
                    target: meta.target,
                    attachment_gen: meta.attachment_gen,
                    refresh_rev: meta.refresh_rev,
                    mutation_fence: meta.mutation_fence,
                    live_stamp: meta.live_stamp,
                    preserved_from: meta.preserved_from,
                    mode: meta.mode,
                    failure_policy: meta.failure_policy,
                    result,
                },
            )));
        })
    }

    /// Apply a finished background transcript rebuild, repair, or refresh.
    /// Rejects stale seq, target mismatch, attachment generation drift, refresh
    /// revision drift, prompt fences, and concurrent live mutations.
    pub(crate) fn apply_history_rebuild(&mut self, completion: HistoryRebuildCompletion) {
        if completion.seq != self.history_rebuild_seq {
            // A newer rebuild owns the abort slot now; leave it alone.
            return;
        }
        self.history_rebuild_abort = None;
        self.history_rebuild_handle = None;
        let current = self.current_history_target();
        if current != completion.target {
            return;
        }
        if !self.completion_fences_allow(&completion) {
            if completion.mode.shows_loading_line() {
                self.remove_history_loading_line(completion.preserved_from);
            }
            self.schedule_history_repair_retry();
            return;
        }
        match completion.result {
            Ok(items) => self.apply_successful_history_rebuild(
                items,
                completion.preserved_from,
                completion.mode,
                completion.mutation_fence,
                current,
            ),
            Err(error) => self.handle_failed_history_rebuild(
                error,
                completion.preserved_from,
                completion.mode,
                completion.failure_policy,
                current,
            ),
        }
    }

    fn completion_fences_allow(&self, completion: &HistoryRebuildCompletion) -> bool {
        if self.is_unsettled_write_blocking_current_target()
            || completion.refresh_rev != self.history_refresh_rev
        {
            return false;
        }
        if !completion.mode.is_shared_attachment_scoped() && !self.is_history_repair_pending() {
            return true;
        }
        completion.attachment_gen == self.session_attachment_gen
            && self.current_prompt_abort.is_none()
            && completion
                .live_stamp
                .as_ref()
                .is_none_or(|stamp| stamp.allows(&self.live_events))
    }

    fn apply_successful_history_rebuild(
        &mut self,
        items: Vec<TranscriptItem>,
        preserved_from: usize,
        mode: RebuildMode,
        mutation_fence: u64,
        current: HistoryTarget,
    ) {
        if (mode.is_shared_attachment_scoped() || self.is_history_repair_pending())
            && mutation_fence != self.transcript_mutation_seq
        {
            if mode.shows_loading_line() {
                self.remove_history_loading_line(preserved_from);
            }
            // Live rows arrived while this snapshot was reading. Discard it
            // rather than overwriting them, and keep repair pending.
            if mode == RebuildMode::RepairInitial || self.is_history_repair_pending() {
                self.history_repair_target = Some(current);
                self.schedule_history_repair_retry();
            }
            return;
        }
        self.commit_rebuilt_transcript(
            items,
            preserved_from,
            mode != RebuildMode::PreserveTail,
            true,
        );
        self.clear_unresolved_navigation_for_current_destination();
        self.history_refresh_rev = self.history_refresh_rev.saturating_add(1);
        self.note_live_transcript_mutation();
        self.history_repair_target = None;
        self.history_repair_uses_wrapper = true;
        self.history_repair_retry_delay = INITIAL_HISTORY_REPAIR_DELAY;
        self.abort_history_repair_retry();
    }

    fn handle_failed_history_rebuild(
        &mut self,
        error: String,
        preserved_from: usize,
        mode: RebuildMode,
        failure_policy: RebuildFailurePolicy,
        current: HistoryTarget,
    ) {
        let failure_policy = self.resolve_rebuild_failure_policy(failure_policy);
        if mode.shows_loading_line()
            && failure_policy != RebuildFailurePolicy::ReplaceWithInitialErrorAndRepair
        {
            self.remove_history_loading_line(preserved_from);
        }
        match failure_policy {
            RebuildFailurePolicy::ReplaceWithInitialErrorAndRepair => {
                self.replace_with_initial_history_error(error, preserved_from, current);
            }
            RebuildFailurePolicy::ShowHandoffErrorAndRepair => {
                self.show_handoff_history_error(error, preserved_from, current);
            }
            RebuildFailurePolicy::PreserveStaleWithErrorAndRepair => {
                self.show_stale_history_error(error, mode, current);
            }
            RebuildFailurePolicy::KeepRepairOrPreserveSilently => {
                self.keep_history_repair_error(error, mode, current);
            }
        }
    }

    fn remove_history_loading_line(&mut self, preserved_from: usize) {
        let status_idx = preserved_from.saturating_sub(1);
        if matches!(
            self.app.transcript.get(status_idx),
            Some(TranscriptItem::StatusLine(text)) if text == LOADING_HISTORY_STATUS
        ) {
            self.app.transcript.remove(status_idx);
        }
    }

    fn replace_with_initial_history_error(
        &mut self,
        error: String,
        preserved_from: usize,
        current: HistoryTarget,
    ) {
        let (error_items, _) = crate::types::Tui::build_initial_transcript_from_history(
            &self.config,
            Err(anyhow::anyhow!(error)),
        );
        self.commit_rebuilt_transcript(error_items, preserved_from, false, false);
        self.clear_unresolved_navigation_for_current_destination();
        self.history_refresh_rev = self.history_refresh_rev.saturating_add(1);
        self.note_live_transcript_mutation();
        self.arm_history_repair(current, true);
    }

    fn show_handoff_history_error(
        &mut self,
        error: String,
        preserved_from: usize,
        current: HistoryTarget,
    ) {
        if self.app.transcript.is_empty() {
            let (error_items, _) = crate::types::Tui::build_initial_transcript_from_history(
                &self.config,
                Err(anyhow::anyhow!(error)),
            );
            self.commit_rebuilt_transcript(error_items, preserved_from, true, false);
            self.history_refresh_rev = self.history_refresh_rev.saturating_add(1);
        } else {
            self.upsert_history_error_line(HISTORY_LOAD_ERROR_PREFIX, &error);
            self.pin_transcript_to_bottom();
        }
        self.note_live_transcript_mutation();
        self.arm_history_repair(current, true);
    }

    fn show_stale_history_error(
        &mut self,
        error: String,
        mode: RebuildMode,
        current: HistoryTarget,
    ) {
        self.retire_transcript_actions_for_pending_mutation();
        self.upsert_history_error_line(STALE_HISTORY_ERROR_PREFIX, &error);
        self.note_live_transcript_mutation();
        self.pin_transcript_to_bottom();
        self.arm_history_repair(current, mode.uses_initial_wrapper());
    }

    fn keep_history_repair_error(
        &mut self,
        error: String,
        mode: RebuildMode,
        current: HistoryTarget,
    ) {
        if mode != RebuildMode::RepairInitial && !self.is_history_repair_pending() {
            return;
        }
        if self.has_history_error_line(STALE_HISTORY_ERROR_PREFIX) {
            self.upsert_history_error_line(STALE_HISTORY_ERROR_PREFIX, &error);
            self.note_live_transcript_mutation();
        } else if self.has_history_error_line(HISTORY_LOAD_ERROR_PREFIX)
            || self.app.transcript.is_empty()
        {
            self.upsert_history_error_line(HISTORY_LOAD_ERROR_PREFIX, &error);
            self.note_live_transcript_mutation();
        }
        self.history_repair_target = Some(current);
        self.schedule_history_repair_retry();
    }

    fn arm_history_repair(&mut self, current: HistoryTarget, uses_wrapper: bool) {
        self.mark_history_repair_pending(current, uses_wrapper);
        self.schedule_history_repair_retry();
    }

    fn mark_history_repair_pending(&mut self, current: HistoryTarget, uses_wrapper: bool) {
        self.history_repair_target = Some(current);
        self.history_repair_uses_wrapper = uses_wrapper;
        self.history_repair_retry_delay = INITIAL_HISTORY_REPAIR_DELAY;
    }

    fn has_history_error_line(&self, prefix: &str) -> bool {
        self.app
            .transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::ErrorText(text) if text.starts_with(prefix)))
    }

    fn upsert_history_error_line(&mut self, prefix: &str, error: &str) {
        let message = format!("{prefix}{error}");
        if let Some(TranscriptItem::ErrorText(existing)) =
            self.app.transcript.iter_mut().rev().find(
                |item| matches!(item, TranscriptItem::ErrorText(text) if text.starts_with(prefix)),
            )
        {
            *existing = message;
            return;
        }
        self.app.transcript.push(TranscriptItem::ErrorText(message));
    }

    fn commit_rebuilt_transcript(
        &mut self,
        items: Vec<TranscriptItem>,
        preserved_from: usize,
        replace: bool,
        mark_actionable: bool,
    ) {
        let preserved: Vec<TranscriptItem> = if replace {
            Vec::new()
        } else {
            self.app
                .transcript
                .get(preserved_from..)
                .map(<[TranscriptItem]>::to_vec)
                .unwrap_or_default()
        };
        self.app.transcript = items;
        self.app.transcript.extend(preserved);
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.streamed_text_idx = None;
        self.app.last_ui_output_source = None;
        let unresolved_destination = self.has_unresolved_navigation_to_current_destination();
        if self.interaction_pending() && !unresolved_destination {
            // While a confirmation is pending on the already-current target,
            // preserve independently loaded views (cid documents, detail
            // snapshots, archive, child views) and restore the guarded tray
            // if no valid view remains (#2953).
            self.normalize_reading_after_transcript_rebuild();
        } else {
            self.clear_transcript_overlays();
            if unresolved_destination {
                // Destination commit or initial-error replacement drops any
                // foreign detached reading views opened from retained rows
                // while the new destination was loading (#2922 DI-PH1).
                self.app.subagent_view_stack.clear();
            }
            if self.interaction_reading() && !self.read_view_active() {
                // Clearing the last foreign read view while a confirmation is
                // pending restores guarded tray focus.
                self.arm_interaction_return_guard();
            }
        }
        // Reset scroll state so the widget doesn't subtract-overflow when
        // the rebuilt transcript is shorter than the previous one.
        self.app.scroll_state = ratatui_widget_scrolling::ScrollState::new();
        self.subagent_rows_dirty = true;
        self.transcript_view_ownership.view_revision = self
            .transcript_view_ownership
            .view_revision
            .saturating_add(1);
        self.transcript_view_ownership.active_binding =
            mark_actionable.then(|| self.current_transcript_action_target());
        self.retire_transcript_confirmation_modal();
        self.reject_stale_queued_history_commands();
        self.pin_transcript_to_bottom();
    }

    pub(crate) fn schedule_history_repair_retry(&mut self) {
        if self.is_unsettled_write_blocking_current_target()
            || self.history_rebuild_abort.is_some()
            || !self.is_history_repair_pending()
            || self.current_prompt_abort.is_some()
        {
            return;
        }
        self.abort_history_repair_retry();
        let delay = self.next_history_repair_delay();
        let target = self.current_history_target();
        let attachment_gen = self.session_attachment_gen;
        let refresh_rev = self.history_refresh_rev;
        let event_tx = self.event_tx.clone();
        self.history_repair_retry_handle = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = event_tx.send(TuiEvent::HistoryRepairRetryDue {
                target,
                attachment_gen,
                refresh_rev,
            });
        }));
    }

    fn next_history_repair_delay(&mut self) -> Duration {
        #[cfg(test)]
        if let Some(override_delay) = self.history_repair_retry_delay_override {
            return override_delay;
        }
        let delay = self.history_repair_retry_delay;
        self.history_repair_retry_delay = delay
            .checked_mul(2)
            .unwrap_or(MAX_HISTORY_REPAIR_DELAY)
            .min(MAX_HISTORY_REPAIR_DELAY);
        delay
    }

    pub(crate) fn handle_history_repair_retry_due(
        &mut self,
        target: HistoryTarget,
        attachment_gen: u64,
        refresh_rev: u64,
    ) {
        self.history_repair_retry_handle = None;
        if !self.is_history_repair_pending()
            || target != self.current_history_target()
            || attachment_gen != self.session_attachment_gen
            || refresh_rev != self.history_refresh_rev
            || self.history_rebuild_abort.is_some()
            || self.current_prompt_abort.is_some()
        {
            return;
        }
        self.schedule_history_repair();
    }

    pub(crate) fn enqueue_history_command(&mut self, line: String) {
        self.enqueue_history_command_with_targets(line, None, None);
    }

    pub(crate) fn enqueue_bound_history_command(
        &mut self,
        line: String,
        expected_target: Option<TranscriptActionTarget>,
    ) {
        self.enqueue_history_command_with_targets(line, expected_target, None);
    }

    pub(crate) fn enqueue_session_history_command(
        &mut self,
        line: String,
        expected_session: SessionCommandTarget,
    ) {
        self.enqueue_history_command_with_targets(line, None, Some(expected_session));
    }

    fn enqueue_history_command_with_targets(
        &mut self,
        line: String,
        expected_target: Option<TranscriptActionTarget>,
        expected_session: Option<SessionCommandTarget>,
    ) {
        if !self
            .validate_history_command_targets(expected_target.as_ref(), expected_session.as_ref())
        {
            self.show_stale_transcript_notice();
            return;
        }
        let cmd = QueuedHistoryCommand {
            line,
            expected_target,
            expected_session,
        };
        if self.history_command_running {
            // Preserve dot-command order: at most one runs at a time.
            self.history_command_queue.push_back(cmd);
            return;
        }
        self.spawn_history_command(cmd);
    }

    fn bound_command_fence_target(
        &self,
        expected_target: Option<&TranscriptActionTarget>,
        expected_session: Option<&SessionCommandTarget>,
    ) -> Option<SessionCommandTarget> {
        if let Some(session) = expected_session {
            return Some(session.clone());
        }
        expected_target.map(|row| SessionCommandTarget {
            target: row.target.clone(),
            storage_key: row.storage_key.clone(),
            target_epoch: self.transcript_view_ownership.session_fence.target_epoch,
        })
    }

    fn command_write_destination(
        &self,
        bound_target: Option<&SessionCommandTarget>,
    ) -> HistoryDestination {
        bound_target
            .map(|b| HistoryDestination {
                target: b.target.clone(),
                storage_key: b.storage_key.clone(),
            })
            .unwrap_or_else(|| self.current_history_destination())
    }

    fn launch_history_command_task(
        &mut self,
        exec_config: harnx_runtime::config::GlobalConfig,
        targets: (Option<TranscriptActionTarget>, Option<SessionCommandTarget>),
        prev_session: Option<String>,
        prev_agent: Option<String>,
        line: String,
    ) {
        let isolated = targets.0.is_some() || targets.1.is_some();
        let abort_signal = create_abort_signal();
        self.history_command_abort = Some(abort_signal.clone());
        let outer_abort = abort_signal.clone();
        let local_worker = self.local_worker.clone();
        let event_tx = self.event_tx.clone();
        #[cfg(test)]
        let command_gate = self.history_command_gate.clone();
        tokio::spawn(async move {
            let (output, result, adopted_sink) = Tui::run_history_command(
                (exec_config, local_worker),
                targets,
                (abort_signal, outer_abort, isolated),
                line.clone(),
                #[cfg(test)]
                command_gate,
            )
            .await;
            let _ = event_tx.send(TuiEvent::HistoryCommandFinished {
                line,
                prev_session,
                prev_agent,
                output,
                result,
                adopted_sink,
            });
        });
    }

    fn spawn_history_command(&mut self, cmd: QueuedHistoryCommand) {
        if !self.validate_history_command_targets(
            cmd.expected_target.as_ref(),
            cmd.expected_session.as_ref(),
        ) {
            self.show_stale_transcript_notice();
            return;
        }
        let QueuedHistoryCommand {
            line,
            expected_target,
            expected_session,
        } = cmd;
        let bound_target =
            self.bound_command_fence_target(expected_target.as_ref(), expected_session.as_ref());
        let exec_config = if bound_target.is_some() {
            std::sync::Arc::new(harnx_runtime::config::ConfigLock::new(
                self.config.read().clone(),
            ))
        } else {
            self.config.clone()
        };
        let write_destination = self.command_write_destination(bound_target.as_ref());
        self.transcript_view_ownership
            .session_fence
            .running_bound_target = bound_target;
        self.begin_history_command(&line, write_destination);
        let (prev_session, prev_agent) = {
            let cfg = self.config.read();
            (
                cfg.session.as_ref().map(|s| s.id().to_string()),
                cfg.active_agent_ref(),
            )
        };
        self.launch_history_command_task(
            exec_config,
            (expected_target, expected_session),
            prev_session,
            prev_agent,
            line,
        );
    }

    fn begin_history_command(&mut self, line: &str, write_destination: HistoryDestination) {
        self.history_command_running = true;
        self.retire_pending_title_rename();
        if !invalidates_transcript_coordinates(line) {
            return;
        }
        self.transcript_view_ownership.unsettled_write_destination = Some(write_destination);
        self.retire_transcript_actions_for_pending_mutation();
        self.cancel_inflight_history_rebuild();
        self.history_rebuild_seq = self.history_rebuild_seq.saturating_add(1);
        self.abort_history_repair_retry();
    }

    pub(crate) fn spawn_title_rename(
        &mut self,
        target: harnx_runtime::config::session::TitleWriteTarget,
        record: harnx_runtime::config::session::TitleRecord,
    ) {
        let (seq, abort) =
            supersede_abort_slot(&mut self.title_rename_abort, &mut self.title_rename_seq);
        let expected_target = self.current_history_target();
        let storage_key = self.current_session_storage_key();
        let title = record.title.clone();
        let live_config = self.config.clone();
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = Tui::run_title_rename(live_config, target, record, abort.clone()).await;
            if abort.aborted() {
                return;
            }
            let _ = event_tx.send(TuiEvent::TitleRenameFinished {
                seq,
                target: expected_target,
                storage_key,
                title,
                result,
            });
        });
    }

    async fn run_title_rename(
        live_config: harnx_runtime::config::GlobalConfig,
        target: harnx_runtime::config::session::TitleWriteTarget,
        record: harnx_runtime::config::session::TitleRecord,
        abort: AbortSignal,
    ) -> Result<()> {
        if !harnx_runtime::config::session::record_title_durably(
            &live_config,
            target,
            record,
            Some(&abort),
        )
        .await?
        {
            anyhow::bail!("The active session changed before its title was set");
        }
        Ok(())
    }

    async fn run_history_command(
        runtime: (
            harnx_runtime::config::GlobalConfig,
            std::sync::Arc<
                tokio::sync::Mutex<
                    Option<harnx_runtime::local_orchestrator::LocalWorkerSupervisor>,
                >,
            >,
        ),
        targets: (Option<TranscriptActionTarget>, Option<SessionCommandTarget>),
        abort_and_isolated: (AbortSignal, AbortSignal, bool),
        line: String,
        #[cfg(test)] command_gate: Option<crate::types::TestHistoryCommandGateFn>,
    ) -> (
        String,
        Result<harnx_runtime::commands::CommandOutcome>,
        Option<AdoptedSessionSink>,
    ) {
        let (exec_config, local_worker) = runtime;
        let (expected_target, expected_session) = targets;
        let (abort_signal, outer_abort, isolated) = abort_and_isolated;
        let mut output = Vec::<u8>::new();
        // Command futures carry agent/session initialization state. Keep that
        // frame off the nested TUI event-dispatch stack.
        let target_mismatch = expected_target
            .as_ref()
            .is_some_and(|expected| !config_matches_action_target(&exec_config, expected))
            || expected_session
                .as_ref()
                .is_some_and(|expected| !config_matches_session_target(&exec_config, expected));
        let result = if target_mismatch {
            Err(anyhow::anyhow!(
                "Session changed before transcript action ran; stale mutation rejected"
            ))
        } else {
            #[cfg(test)]
            let abort_signal = if let Some(gate) = command_gate {
                gate(line.clone(), abort_signal).await
            } else {
                abort_signal
            };
            Box::pin(
                harnx_runtime::commands::run_command_with_output_and_local_worker(
                    &exec_config,
                    abort_signal,
                    &line,
                    &mut output,
                    &local_worker,
                ),
            )
            .await
        };
        let adopted_sink = if isolated && result.is_ok() && !outer_abort.aborted() {
            harnx_runtime::config::session::active_session_sink_with_storage_key(&exec_config)
        } else {
            None
        };
        (
            String::from_utf8_lossy(&output).into_owned(),
            result,
            adopted_sink,
        )
    }

    fn settle_history_mutation(
        &mut self,
        finishing_dest: Option<&HistoryDestination>,
    ) -> Option<HistoryDestination> {
        if finishing_dest.is_some_and(|dest| {
            self.transcript_view_ownership
                .unsettled_write_destination
                .as_ref()
                != Some(dest)
        }) {
            return None;
        }
        // A failed or cancelled write can still have changed durable history.
        let settled = self
            .transcript_view_ownership
            .unsettled_write_destination
            .take()?;
        if settled == self.current_history_destination() {
            self.mark_history_repair_pending(settled.target.clone(), true);
        }
        Some(settled)
    }

    fn settle_finished_session_fence(
        &mut self,
        line: &str,
        result: Result<harnx_runtime::commands::CommandOutcome>,
        adopted_sink: Option<AdoptedSessionSink>,
    ) -> (
        bool,
        Option<HistoryDestination>,
        Result<harnx_runtime::commands::CommandOutcome>,
    ) {
        let Some(expected) = self
            .transcript_view_ownership
            .session_fence
            .running_bound_target
            .take()
        else {
            return (false, None, result);
        };
        if expected != self.current_session_command_target() {
            let expected_dest = HistoryDestination {
                target: expected.target,
                storage_key: expected.storage_key,
            };
            return (true, Some(expected_dest), result);
        }
        let result = match (result, session_clear_kind(line)) {
            (Ok(outcome), Some(clear_kind)) => {
                apply_live_session_clear(&self.config, clear_kind).map(|()| outcome)
            }
            (res, _) => res,
        };
        if result.is_ok() {
            if let Some((sink_storage_key, sink)) = adopted_sink {
                if expected.storage_key.as_deref() == Some(sink_storage_key.as_str()) {
                    harnx_runtime::config::session::install_session_sink_if_missing(
                        &self.config,
                        &sink_storage_key,
                        sink,
                    );
                }
            }
        }
        (false, None, result)
    }

    async fn reconcile_stale_history_command(
        &mut self,
        stale_destination: Option<HistoryDestination>,
        prev_session: Option<String>,
        prev_agent: Option<String>,
        line: &str,
    ) {
        if stale_destination.as_ref() == Some(&self.current_history_destination()) {
            self.reconcile_transcript_after_command(prev_session, prev_agent, line)
                .await;
        }
    }

    async fn finish_active_history_command(
        &mut self,
        line: String,
        prev_session: Option<String>,
        prev_agent: Option<String>,
        output: String,
        result: Result<harnx_runtime::commands::CommandOutcome>,
    ) {
        let clean = history_command_display_output(&output);
        self.finish_command(
            result,
            clean,
            (&line, prev_session, prev_agent, is_mutation_command(&line)),
        )
        .await;
    }

    async fn apply_finished_history_command(
        &mut self,
        stale_fence: (bool, Option<HistoryDestination>),
        line: String,
        prev_target: (Option<String>, Option<String>),
        output: String,
        result: Result<harnx_runtime::commands::CommandOutcome>,
    ) {
        let (stale_session, stale_destination) = stale_fence;
        let (prev_session, prev_agent) = prev_target;
        if stale_session {
            self.reconcile_stale_history_command(
                stale_destination,
                prev_session,
                prev_agent,
                &line,
            )
            .await;
        } else {
            self.finish_active_history_command(line, prev_session, prev_agent, output, result)
                .await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn finish_history_command(
        &mut self,
        line: String,
        prev_target: (Option<String>, Option<String>),
        output: String,
        result: Result<harnx_runtime::commands::CommandOutcome>,
    ) {
        self.finish_history_command_with_sink(line, prev_target, output, (result, None))
            .await;
    }

    async fn finish_history_command_with_sink(
        &mut self,
        line: String,
        prev_target: (Option<String>, Option<String>),
        output: String,
        settlement: (
            Result<harnx_runtime::commands::CommandOutcome>,
            Option<AdoptedSessionSink>,
        ),
    ) {
        let (result, adopted_sink) = settlement;
        self.history_command_running = false;
        self.history_command_abort = None;
        let (stale_session, stale_destination, result) =
            self.settle_finished_session_fence(&line, result, adopted_sink);
        let settled_write = self.settle_history_mutation(stale_destination.as_ref());
        self.apply_finished_history_command(
            (stale_session, stale_destination.or(settled_write)),
            line,
            prev_target,
            output,
            result,
        )
        .await;
        self.start_next_history_command();
    }

    fn finish_title_rename(
        &mut self,
        seq: u64,
        target: HistoryTarget,
        storage_key: Option<String>,
        title: String,
        result: Result<()>,
    ) {
        if seq != self.title_rename_seq {
            return;
        }
        self.title_rename_abort = None;
        if target != self.current_history_target()
            || storage_key != self.current_session_storage_key()
        {
            return;
        }
        match result {
            Ok(()) => {
                let _ = std::io::stdout().execute(crossterm::terminal::SetTitle(&title));
                self.refresh_input_chrome();
            }
            Err(err) => {
                self.app
                    .transcript
                    .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
                self.note_live_transcript_mutation();
            }
        }
    }

    fn start_next_history_command(&mut self) {
        while let Some(next) = self.history_command_queue.pop_front() {
            if !self.validate_history_command_targets(
                next.expected_target.as_ref(),
                next.expected_session.as_ref(),
            ) {
                self.show_stale_transcript_notice();
                continue;
            }
            self.spawn_history_command(next);
            return;
        }
    }

    /// Schedule the session-picker refresh as cancellable background work
    /// (design 9): listing sessions can wait on managed-local acquisition.
    /// A newer refresh supersedes an in-flight one; the completion lands as
    /// `PickerSessionsLoaded` and applies only while a picker is open.
    pub(crate) fn spawn_picker_sessions_refresh(&mut self) {
        let (seq, abort) =
            supersede_abort_slot(&mut self.picker_refresh_abort, &mut self.picker_refresh_seq);
        let picker_incarnation = self.picker_incarnation;
        let target = self.current_picker_target();
        self.picker_refresh_target = Some(target.clone());
        let config = self.config.clone();
        let event_tx = self.event_tx.clone();
        #[cfg(test)]
        let picker_override = self.picker_sessions_override.clone();
        tokio::spawn(async move {
            let (sessions, fetch_error) = tokio::select! {
                biased;
                _ = wait_abort_signal(&abort) => return,
                loaded = load_picker_sessions(
                    &config,
                    &abort,
                    #[cfg(test)]
                    picker_override,
                ) => loaded,
            };
            if abort.aborted_ctrlc() {
                return;
            }
            let _ = event_tx.send(TuiEvent::PickerSessionsLoaded {
                seq,
                picker_incarnation,
                target,
                sessions,
                fetch_error,
            });
        });
    }

    /// Apply a finished background picker refresh: only an open session
    /// picker consumes it. The highlighted session keeps its highlight in
    /// the new list (falling back to a clamped index); a closed or replaced
    /// modal discards the result.
    pub(crate) fn apply_picker_sessions(
        &mut self,
        sessions: Vec<harnx_runtime::config::SessionMeta>,
        fetch_error: Option<String>,
    ) {
        let picker = {
            let Some(crate::types::ModalState::SessionPicker {
                selected,
                origin_agent,
                origin_session,
                sessions: old_sessions,
                ..
            }) = self.app.modal.as_ref()
            else {
                return;
            };
            let highlighted = old_sessions
                .get(selected.saturating_sub(1))
                .map(|session| session.id.clone());
            (
                *selected,
                highlighted,
                origin_agent.clone(),
                origin_session.clone(),
            )
        };
        let (selected, highlighted, origin_agent, origin_session) = picker;
        let selected = highlighted
            .and_then(|id| {
                sessions
                    .iter()
                    .position(|session| session.id == id)
                    .map(|index| index + 1)
            })
            .unwrap_or_else(|| selected.min(sessions.len()));
        self.app.modal = Some(crate::types::ModalState::SessionPicker {
            sessions,
            selected,
            origin_agent,
            origin_session,
            error: fetch_error,
        });
    }

    /// Schedule the picker's "New session" reservation as cancellable
    /// background work (design 9): reserving a session id can wait on
    /// managed-local acquisition. Completion lands as `NewSessionReserved`
    /// or `NewSessionReservationFailed`. A newer reservation supersedes an
    /// in-flight one (its signal fires, its completion is discarded by the
    /// seq fence), and a cancelled reservation emits nothing — Ctrl-C must
    /// not reopen the picker with a cancellation error.
    pub(crate) fn spawn_new_session_reservation(
        &mut self,
        restore: crate::lifecycle::NewSessionSelection,
    ) {
        if let Some(previous) = self.new_session_reservation_abort.take() {
            previous.set_ctrlc();
        }
        self.new_session_reservation_seq = self.new_session_reservation_seq.saturating_add(1);
        let seq = self.new_session_reservation_seq;
        let crate::lifecycle::NewSessionSelection {
            sessions,
            selected,
            origin_agent,
            origin_session,
        } = restore;
        let config = self.config.clone();
        let event_tx = self.event_tx.clone();
        let abort = create_abort_signal();
        self.new_session_reservation_abort = Some(abort.clone());
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = wait_abort_signal(&abort) => return,
                result = harnx_runtime::config::Config::reserve_new_session_id_with_abort(
                    &config,
                    Some(&abort),
                ) => result,
            };
            // Both the select above and the reservation's own wait watch the
            // same signal; whichever loses the race, a cancelled job is over.
            if abort.aborted_ctrlc() {
                return;
            }
            match result {
                Ok(session_id) => {
                    let _ = event_tx.send(TuiEvent::NewSessionReserved {
                        seq,
                        session_id,
                        origin_agent,
                        origin_session,
                    });
                }
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::NewSessionReservationFailed {
                        seq,
                        error: format!("{error:#}"),
                        sessions,
                        selected,
                        origin_agent,
                        origin_session,
                    });
                }
            }
        });
    }

    /// Schedule the session-picker unread toggle (`u`/`U`) as cancellable
    /// background work (design 9): both the metadata-store lookup and write
    /// run under the job's abort select so Ctrl-C drops a waiting lookup
    /// immediately. A newer toggle supersedes an in-flight one; the completion
    /// re-lists only the picker that is still the current view.
    pub(crate) fn spawn_picker_unread_toggle(
        &mut self,
        cluster: String,
        storage_key: String,
        mark_unread: bool,
    ) {
        if let Some(previous) = self.picker_unread_toggle_abort.take() {
            previous.set_ctrlc();
        }
        self.picker_unread_toggle_seq = self.picker_unread_toggle_seq.saturating_add(1);
        let seq = self.picker_unread_toggle_seq;
        let config = self.config.read().clone();
        let event_tx = self.event_tx.clone();
        let abort = create_abort_signal();
        self.picker_unread_toggle_abort = Some(abort.clone());
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = wait_abort_signal(&abort) => return,
                result = Tui::run_picker_unread_toggle(
                    &config,
                    &cluster,
                    &storage_key,
                    mark_unread,
                    &abort,
                ) => result,
            };
            if abort.aborted_ctrlc() {
                return;
            }
            if let Err(error) = result {
                log::warn!("Failed to update session unread state in picker: {error:#}");
            }
            let _ = event_tx.send(TuiEvent::PickerUnreadToggleFinished { seq });
        });
    }

    async fn run_picker_unread_toggle(
        config: &harnx_runtime::config::Config,
        cluster: &str,
        storage_key: &str,
        mark_unread: bool,
        abort: &harnx_runtime::utils::AbortSignal,
    ) -> Result<()> {
        let client = config.nats_client_with_abort(cluster, Some(abort)).await?;
        let jetstream = async_nats::jetstream::new(client);
        let store =
            harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1)
                .await?;
        if mark_unread {
            store.mark_unread(storage_key).await?;
        } else {
            store.mark_read(storage_key).await?;
        }
        Ok(())
    }

    /// Schedule the `.info session` / `.dump session` overlay load as
    /// cancellable background work (design 9): the session log reads can
    /// wait on managed-local acquisition. A newer info command supersedes
    /// an in-flight one; completion lands as `InfoOverlayLoaded`.
    pub(crate) fn spawn_info_overlay_load(
        &mut self,
        tokens: Vec<String>,
        info_type: crate::input_commands::InfoOverlayType,
    ) {
        let (seq, abort) =
            supersede_abort_slot(&mut self.info_overlay_abort, &mut self.info_overlay_seq);
        let target = self.current_history_target();
        let storage_key = self.current_session_storage_key();
        let config = self.config.clone();
        let event_tx = self.event_tx.clone();
        #[cfg(test)]
        let info_override = self.info_overlay_override.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = wait_abort_signal(&abort) => return,
                rendered = load_info_overlay(
                    &config,
                    &tokens,
                    &info_type,
                    &abort,
                    #[cfg(test)]
                    info_override,
                ) => rendered,
            };
            if abort.aborted_ctrlc() {
                return;
            }
            let display_text = result.unwrap_or_else(|err| format!("Error: {err}"));
            let _ = event_tx.send(TuiEvent::InfoOverlayLoaded {
                seq,
                target,
                storage_key,
                display_text,
                info_type,
            });
        });
    }

    /// Completions of the other broker-waiting background jobs (picker
    /// refresh, picker "New session" reservation, info-overlay loads)
    /// apply through the event loop like history work, never inline. A
    /// completion clears its job's abort slot only after verifying the
    /// completion still owns the slot (matching request `seq` and target/picker
    /// incarnation), so stale completions never disarm newer work.
    // qual:allow(iosp) reason: "each arm validates ownership, clears its job's slot, then delegates to the apply call that owns the result; that fence-clear-apply sequence is the dispatch contract"
    pub(crate) async fn handle_background_work_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::PickerSessionsLoaded {
                seq,
                picker_incarnation,
                target,
                sessions,
                fetch_error,
            } => {
                if seq != self.picker_refresh_seq
                    || picker_incarnation != self.picker_incarnation
                    || target != self.current_picker_target()
                {
                    return;
                }
                self.picker_refresh_abort = None;
                self.picker_refresh_target = None;
                self.apply_picker_sessions(sessions, fetch_error);
            }
            TuiEvent::NewSessionReserved {
                seq,
                session_id,
                origin_agent,
                origin_session,
            } => {
                // Stale completion (superseded or cancelled job): a newer
                // reservation owns the slot; leave it alone and do not
                // switch the session.
                if seq != self.new_session_reservation_seq {
                    return;
                }
                self.new_session_reservation_abort = None;
                self.apply_new_session_reserved(session_id, origin_agent, origin_session)
                    .await;
            }
            TuiEvent::NewSessionReservationFailed {
                seq,
                error,
                sessions,
                selected,
                origin_agent,
                origin_session,
            } => {
                if seq != self.new_session_reservation_seq {
                    return;
                }
                self.new_session_reservation_abort = None;
                self.apply_new_session_reservation_failed(
                    error,
                    sessions,
                    selected,
                    origin_agent,
                    origin_session,
                );
            }
            TuiEvent::PickerUnreadToggleFinished { seq } => {
                if seq != self.picker_unread_toggle_seq {
                    return;
                }
                self.picker_unread_toggle_abort = None;
                // Only the still-open picker view re-lists; the refresh is
                // itself fenced to an open picker by apply_picker_sessions.
                if matches!(
                    self.app.modal,
                    Some(crate::types::ModalState::SessionPicker { .. })
                ) {
                    self.spawn_picker_sessions_refresh();
                }
            }
            TuiEvent::InfoOverlayLoaded {
                seq,
                target,
                storage_key,
                display_text,
                info_type,
            } => {
                if seq != self.info_overlay_seq
                    || target != self.current_history_target()
                    || storage_key != self.current_session_storage_key()
                {
                    return;
                }
                self.info_overlay_abort = None;
                self.open_info_overlay(display_text, info_type);
            }
            // The dispatch groups the events; nothing else reaches here.
            _ => {}
        }
    }

    /// Background history work (rebuild, repair, refresh, retry, or queued
    /// dot command) applies through the event loop, never inline on the task
    /// that produced it.
    pub(crate) async fn handle_history_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::HistoryRebuildFinished(completion) => {
                self.apply_history_rebuild(*completion);
            }
            TuiEvent::HistoryRepairRetryDue {
                target,
                attachment_gen,
                refresh_rev,
            } => {
                self.handle_history_repair_retry_due(target, attachment_gen, refresh_rev);
            }
            TuiEvent::HistoryCommandFinished {
                line,
                prev_session,
                prev_agent,
                output,
                result,
                adopted_sink,
            } => {
                self.finish_history_command_with_sink(
                    line,
                    (prev_session, prev_agent),
                    output,
                    (result, adopted_sink),
                )
                .await;
            }
            TuiEvent::TitleRenameFinished {
                seq,
                target,
                storage_key,
                title,
                result,
            } => {
                self.finish_title_rename(seq, target, storage_key, title, result);
            }
            // The dispatch groups the events; nothing else reaches here.
            _ => {}
        }
    }
}

async fn load_snapshot_items(
    config: &harnx_runtime::config::GlobalConfig,
    #[cfg_attr(not(test), allow(unused_variables))] mode: RebuildMode,
    uses_initial_wrapper: bool,
    abort: &harnx_runtime::utils::AbortSignal,
    #[cfg(test)] load_override: Option<crate::types::TestHistoryLoadFn>,
) -> Result<Vec<TranscriptItem>, String> {
    #[cfg(test)]
    let raw = if let Some(override_fn) = load_override {
        override_fn(config.clone(), mode, abort.clone()).await
    } else {
        crate::lifecycle::load_session_history_transcript_items(config, Some(abort)).await
    };
    #[cfg(not(test))]
    let raw = crate::lifecycle::load_session_history_transcript_items(config, Some(abort)).await;

    let items = raw.map_err(|error| format!("{error:#}"))?;
    if uses_initial_wrapper {
        Ok(Tui::build_initial_transcript_from_history(config, Ok(items)).0)
    } else {
        Ok(items)
    }
}

fn supersede_abort_slot(slot: &mut Option<AbortSignal>, seq: &mut u64) -> (u64, AbortSignal) {
    if let Some(previous) = slot.take() {
        previous.set_ctrlc();
    }
    *seq = seq.saturating_add(1);
    let abort = create_abort_signal();
    *slot = Some(abort.clone());
    (*seq, abort)
}

fn retire_abort_slot(slot: &mut Option<AbortSignal>, seq: &mut u64) {
    if let Some(previous) = slot.take() {
        previous.set_ctrlc();
    }
    *seq = seq.saturating_add(1);
}

async fn load_picker_sessions(
    config: &harnx_runtime::config::GlobalConfig,
    abort: &AbortSignal,
    #[cfg(test)] picker_override: Option<crate::types::TestPickerSessionsFn>,
) -> (Vec<harnx_runtime::config::SessionMeta>, Option<String>) {
    #[cfg(test)]
    if let Some(override_fn) = picker_override {
        return override_fn(config.clone(), abort.clone()).await;
    }
    #[cfg(not(test))]
    let _ = abort;
    Tui::picker_sessions(config).await
}

async fn load_info_overlay(
    config: &harnx_runtime::config::GlobalConfig,
    tokens: &[String],
    info_type: &crate::input_commands::InfoOverlayType,
    abort: &AbortSignal,
    #[cfg(test)] info_override: Option<crate::types::TestInfoOverlayFn>,
) -> Result<String> {
    #[cfg(test)]
    if let Some(override_fn) = info_override {
        return override_fn(config.clone(), tokens.to_vec(), *info_type, abort.clone()).await;
    }
    #[cfg(not(test))]
    let _ = abort;
    Tui::render_info_overlay(config, tokens, info_type).await
}
