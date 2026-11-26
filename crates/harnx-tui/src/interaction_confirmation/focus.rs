//! Focus-aware routing while a pending confirmation coexists with read views.
//!
//! A `ConfirmInteraction` tray no longer owns the keyboard unconditionally:
//! when `App::interaction_focus` is [`InteractionFocus::Reading`], keys go to
//! the active read view (browse/detail/cid document/subagent view/archive)
//! and the tray only renders as a compact hint (plan #2927, decisions 2-7).

use crate::types::{InteractionFocus, ModalState, Tui};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

// qual:allow(coupling, oi) reason: "Focus-aware routing extends the shared Tui, following the per-concern module split of interaction_confirmation."
impl Tui {
    /// A durable confirmation tray is pending, regardless of focus.
    pub(crate) fn interaction_pending(&self) -> bool {
        matches!(self.app.modal, Some(ModalState::ConfirmInteraction(_)))
    }

    /// The pending tray is present and a read view holds keyboard focus.
    pub(crate) fn interaction_reading(&self) -> bool {
        self.interaction_pending() && self.app.interaction_focus == InteractionFocus::Reading
    }

    /// Any exclusive read view is open and can hold reading focus. Root
    /// browsing counts only with a live focused row: a browse flag without
    /// a selection is stale (e.g. a rebuild invalidated the row) and must
    /// not adopt or retain reading ownership (review B2).
    pub(crate) fn read_view_active(&self) -> bool {
        self.app.detail_view_open
            || self.app.archive_browser.is_some()
            || !self.app.subagent_view_stack.is_empty()
            || (self.app.transcript_browsing && self.app.transcript_focus.is_some())
    }

    /// Refusal notice for read-only-guarded actions while a confirmation is
    /// pending; applies even when `llm_busy` is false.
    pub(crate) fn show_pending_interaction_notice(&mut self) {
        self.app
            .show_footer_notice("Answer the pending confirmation first");
    }

    /// Refuse composer/transcript mutations while a confirmation is pending,
    /// regardless of llm_busy (plan decision 6). True when the caller must
    /// stop; the notice is already shown.
    pub(crate) fn refuse_when_pending_interaction(&mut self) -> bool {
        if self.interaction_pending() {
            self.show_pending_interaction_notice();
            true
        } else {
            false
        }
    }

    /// Restore tray focus from reading and arm the return-safety guard: for
    /// the next two seconds of quiet keys, decision keys are refused without
    /// sending a decision (binding review clarification).
    pub(crate) fn arm_interaction_return_guard(&mut self) {
        self.app.interaction_focus = InteractionFocus::Tray;
        if let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() {
            let now = Instant::now();
            state.last_key_at = now;
            state.decision_guard_until =
                Some(now + crate::tool_confirmation::TOOL_CONFIRM_IDLE_GATE);
        }
    }

    /// Key routing while the pending confirmation is unfocused: Ctrl+C keeps
    /// its interruption semantics, decision impulses are consumed with a
    /// notice, everything else reaches the active read view.
    pub(crate) async fn handle_pending_reading_key(&mut self, key: KeyEvent) -> Result<()> {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                // Ctrl+C retains the tray's interaction/child interruption
                // semantics even while reading.
                self.handle_interaction_key(key).await;
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                // Ctrl+D is a denial only while the tray is focused; while
                // reading it can neither deny nor exit the TUI.
                self.app
                    .show_footer_notice("Ctrl+D answers in the tray; Esc returns there");
            }
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                // Opening another confirmation while one is pending.
                self.show_pending_interaction_notice();
            }
            _ => self.dispatch_reading_view_key(key).await?,
        }
        self.settle_reading_after_key();
        Ok(())
    }

    // qual:allow(iosp) reason: "Read-view dispatch routes by which view is open, one arm per view; the order encodes which view owns the key, and splitting the arms only moves the finding."
    async fn dispatch_reading_view_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.app.detail_view_open {
            return self.handle_detail_view_key(key).await;
        }
        if self.app.archive_browser.is_some() {
            return self.handle_archive_key(key);
        }
        if !self.app.subagent_view_stack.is_empty() {
            self.handle_subagent_view_key(key).await;
            return Ok(());
        }
        if self.app.transcript_browsing {
            return self.handle_browsing_key(key).await;
        }
        Ok(())
    }

    /// Closing the last read view level returns the pending tray, guarded.
    /// The check runs after each dispatched read-view key, so it covers the
    /// detail, subagent and archive views' own Esc handling.
    fn settle_reading_after_key(&mut self) {
        if self.interaction_reading() && !self.read_view_active() {
            self.arm_interaction_return_guard();
        }
    }

    /// Normalize read focus after a transcript rebuild (refresh or
    /// compaction): independently loaded views stay valid — cid documents
    /// with their history, archive browsers, detail snapshots and child views
    /// — while root-row selection (browse focus, root detail pane) is
    /// invalidated by the rebuild. Ownership is only re-decided when Reading
    /// already held it: a tray that owns focus (e.g. an explicit child
    /// Answer over a retained child stack) keeps its focus and any active
    /// return-guard deadline, because a background rebuild is not a
    /// reading→tray transition (review B1).
    pub(crate) fn normalize_reading_after_transcript_rebuild(&mut self) {
        // A root detail pane renders the focused transcript rows, which the
        // rebuild just invalidated. Doc views, passive detail entries and
        // detail texts are independently loaded and survive.
        if self.app.detail_view_open
            && self.app.doc_view.is_none()
            && self.app.detail_view_entry.is_none()
            && self.app.detail_view_text.is_none()
        {
            self.app.detail_view_open = false;
            self.app.detail_view_title = None;
        }
        self.app.transcript_browsing = false;
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        if self.interaction_pending()
            && self.app.interaction_focus == InteractionFocus::Reading
            && !self.read_view_active()
        {
            // The last read view closed with the rebuild: restore the
            // guarded tray.
            self.arm_interaction_return_guard();
        }
    }
}
