//! Key handling of the interaction tray: decisions, the reading return
//! guard, and Esc/Up unfocus routing to retained views or browsing.

use super::{InteractionModal, Tui};
use crate::tool_confirmation::TOOL_CONFIRM_IDLE_GATE;
use crate::types::{InteractionFocus, ModalState};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::Instant;

const SCROLL_PAGE_LINES: u16 = 10;

// qual:allow(coupling, oi) reason: "Rendering and key handling extend the shared TUI, following the existing per-concern modules."
impl Tui {
    pub(crate) fn paste_interaction_note(&mut self, text: &str) -> bool {
        if self.interaction_reading() {
            // Paste must not edit the hidden note or composer while a read
            // view holds focus; consume it with a notice instead.
            self.show_pending_interaction_notice();
            return true;
        }
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return false;
        };
        insert_pasted_text(state, text);
        true
    }

    pub(crate) async fn handle_interaction_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.handle_interaction_ctrl_c().await;
            return;
        }
        // Return-safety guard first: while it holds, every key restarts its
        // quiet clock and decision keys are refused without a decision.
        if self.interaction_key_refused(&key) {
            return;
        }
        if self.interaction_tray_unfocus_key(&key) {
            return;
        }
        let now = Instant::now();
        if self.interaction_tray_has_a2ui() {
            self.handle_a2ui_interaction_key(key, now);
        } else {
            self.handle_confirm_interaction_key(key, now);
        }
    }

    /// Ctrl+C keeps its interruption semantics: interrupt the pending child
    /// confirmation when one is active, otherwise cancel normally with the
    /// tray cleared.
    async fn handle_interaction_ctrl_c(&mut self) {
        if self.interrupt_child_confirmation() {
            return;
        }
        self.clear_interaction_modal();
        // The durable observer can beat the activity monitor on attach.
        self.active_remote_session = self.interactions.target.clone();
        self.handle_ctrl_c().await;
    }

    /// Decisions accept only reported Press events; Repeat never submits
    /// (legacy terminals auto-repeat as Press, which the guard covers).
    /// While the return guard holds, decision keys are refused outright.
    fn interaction_key_refused(&mut self, key: &KeyEvent) -> bool {
        let (submitting, decision_key, guarding) = self.interaction_key_guard_state(key);
        if submitting {
            return true;
        }
        if guarding && decision_key {
            return true;
        }
        decision_key && key.kind != KeyEventKind::Press
    }

    /// Esc, or Up from an empty note, unfocuses the tray (plan R1); other
    /// keys return false.
    fn interaction_tray_unfocus_key(&mut self, key: &KeyEvent) -> bool {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, KeyModifiers::NONE) => {}
            // R1: Up from an empty note enters transcript browsing, matching
            // the empty composer; A2UI Up stays form navigation.
            (KeyCode::Up, KeyModifiers::NONE) if self.interaction_tray_note_is_blank() => {}
            _ => return false,
        }
        self.unfocus_interaction_tray();
        true
    }

    /// Classify `key` against the pending tray: submitting state, whether the
    /// key is a decision, and whether the reading return guard is holding.
    /// While the guard holds, every key restarts its quiet clock.
    fn interaction_key_guard_state(&mut self, key: &KeyEvent) -> (bool, bool, bool) {
        let now = Instant::now();
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return (false, false, false);
        };
        if state.submitting {
            return (true, false, false);
        }
        let decision_key = interaction_decision_key(state, key);
        let mut guarding = false;
        if let Some(deadline) = state.decision_guard_until {
            if now < deadline {
                // Intervening keys and pastes — refused decision keys and
                // reported Repeat events included — restart the quiet clock.
                state.decision_guard_until = Some(now + TOOL_CONFIRM_IDLE_GATE);
                state.last_key_at = now;
                guarding = true;
            } else {
                state.decision_guard_until = None;
            }
        }
        (false, decision_key, guarding)
    }

    fn interaction_tray_note_is_blank(&self) -> bool {
        matches!(&self.app.modal, Some(ModalState::ConfirmInteraction(state))
            if state.a2ui.is_none() && state.note.is_empty())
    }

    fn interaction_tray_has_a2ui(&self) -> bool {
        matches!(&self.app.modal, Some(ModalState::ConfirmInteraction(state)) if state.a2ui.is_some())
    }

    /// Esc or empty-note Up from the focused tray: resume the retained read
    /// view if one is open (e.g. behind an explicit child Answer), otherwise
    /// enter transcript browsing. Note and form state are never mutated.
    fn unfocus_interaction_tray(&mut self) {
        if !self.interaction_pending() {
            return;
        }
        if self.read_view_active() {
            self.app.interaction_focus = InteractionFocus::Reading;
            return;
        }
        if self.enter_transcript_browsing() {
            self.app.interaction_focus = InteractionFocus::Reading;
        } else {
            self.app.show_footer_notice("Nothing to browse");
        }
    }

    fn handle_a2ui_interaction_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        let Some(form) = &mut state.a2ui else {
            return;
        };
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.submit_a2ui_interaction("deny".into());
                return;
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                state.last_key_at = now;
                form.focus_next();
                return;
            }
            (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => {
                state.last_key_at = now;
                form.focus_prev();
                return;
            }
            (KeyCode::PageUp, _) => {
                state.scroll = state.scroll.saturating_sub(SCROLL_PAGE_LINES);
                return;
            }
            (KeyCode::PageDown, _) => {
                state.scroll = state.scroll.saturating_add(SCROLL_PAGE_LINES);
                return;
            }
            _ => {}
        }

        if form.focus == super::a2ui::FocusTarget::Note {
            self.handle_a2ui_note_key(key, now);
            return;
        }

        state.last_key_at = now;
        if let Some(super::a2ui::A2UIAction::Submit(name)) = form.handle_key(key) {
            self.submit_a2ui_interaction(name);
        }
    }

    fn handle_a2ui_note_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        let primary = state.a2ui.as_ref().and_then(|f| f.primary_action());
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                state.last_key_at = now;
                state.note.insert_newline();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if let Some(action) = primary {
                    self.submit_a2ui_interaction(action);
                }
            }
            _ => {
                state.last_key_at = now;
                state.note.input(key);
            }
        }
    }

    fn handle_confirm_interaction_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(ModalState::ConfirmInteraction(state)) = self.app.modal.as_mut() else {
            return;
        };
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if now.saturating_duration_since(state.last_key_at) >= TOOL_CONFIRM_IDLE_GATE {
                    self.submit_interaction(true);
                }
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => self.submit_interaction(false),
            (KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                state.last_key_at = now;
                state.note.insert_newline();
            }
            (KeyCode::PageUp, _) => state.scroll = state.scroll.saturating_sub(SCROLL_PAGE_LINES),
            (KeyCode::PageDown, _) => state.scroll = state.scroll.saturating_add(SCROLL_PAGE_LINES),
            _ => {
                state.last_key_at = now;
                state.note.input(key);
            }
        }
    }
}

fn insert_pasted_text(state: &mut InteractionModal, text: &str) {
    if state.submitting {
        return;
    }
    let now = Instant::now();
    // Paste is an intervening input: it restarts the return-safety guard's
    // quiet clock while that guard holds.
    if let Some(deadline) = state.decision_guard_until {
        state.decision_guard_until = if now < deadline {
            Some(now + TOOL_CONFIRM_IDLE_GATE)
        } else {
            None
        };
    }
    state.last_key_at = now;
    let pasted_into_form = state
        .a2ui
        .as_mut()
        .is_some_and(|form| form.paste_text(text));
    if !pasted_into_form {
        state.note.insert_str(text);
    }
}

/// Whether this key would submit a decision on the focused tray: approval,
/// denial, or an A2UI button/primary submission. Enter with Shift/Alt only
/// inserts a newline, and Enter on a choice picker only toggles a highlight.
fn interaction_decision_key(state: &InteractionModal, key: &KeyEvent) -> bool {
    match (key.code, key.modifiers) {
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => true,
        (KeyCode::Enter, KeyModifiers::NONE) => match state.a2ui.as_ref() {
            Some(form) => enter_is_decision(form),
            None => true,
        },
        (KeyCode::Char(' '), KeyModifiers::NONE) => matches!(
            state.a2ui.as_ref().map(|form| form.focus),
            Some(super::a2ui::FocusTarget::Button(_))
        ),
        _ => false,
    }
}

/// Enter on a text field or the note submits the primary action, on a button
/// it activates the button, and on a choice picker it only toggles a
/// highlight — so only the first two count as a decision.
fn enter_is_decision(form: &super::a2ui::A2UIFormState) -> bool {
    match form.focus {
        super::a2ui::FocusTarget::Field(idx) => !matches!(
            form.fields.get(idx),
            Some(super::a2ui::FormField::Choice(_))
        ),
        super::a2ui::FocusTarget::Button(_) | super::a2ui::FocusTarget::Note => true,
    }
}
