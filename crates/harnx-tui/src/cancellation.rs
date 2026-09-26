//! Cancellation is operational state, independent of transcript completion.
use crate::types::Tui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_runtime::nats_session::InterruptOutcome;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

pub(crate) enum CancellationPhase {
    Requesting,
    Failed(String),
}

pub(crate) struct CancellationTray {
    pub phase: CancellationPhase,
    pub session_id: String,
    pub cluster: String,
    pub editor_restored: bool,
}

impl Tui {
    pub(crate) fn has_root_cancellation(&self) -> bool {
        self.cancellation.is_some()
    }

    pub(crate) fn cancellation_editor_restored(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|tray| tray.editor_restored)
    }

    /// Whether the busy spinner should hold on a static glyph instead of
    /// animating. A `Failed` interrupt is stalled on the user's own
    /// retry/exit decision; `Requesting` always resolves on its own, so it
    /// keeps animating.
    pub(crate) fn cancellation_failed(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|tray| matches!(tray.phase, CancellationPhase::Failed(_)))
    }

    pub(crate) fn handle_cancellation_or_child_key(&mut self, key: KeyEvent) -> bool {
        if self.handle_cancellation_key(key) {
            return true;
        }
        if self.app.modal.is_some() {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.cancel_selected_child(),
            _ => false,
        }
    }

    /// Start (or retry) a durable interrupt for `session_id`/`cluster`. A
    /// retry preserves `editor_restored` from the tray it replaces — Esc
    /// pressed before a Ctrl+C retry stays pressed across the retry.
    pub(crate) fn start_cancellation(&mut self, session_id: String, cluster: String) {
        let editor_restored = self
            .cancellation
            .as_ref()
            .is_some_and(|tray| tray.editor_restored);
        self.pending_exit_cancel = Some(tokio::spawn((self.exit_cancel_factory)(
            self.config.clone(),
            self.local_worker.clone(),
            session_id.clone(),
            cluster.clone(),
        )));
        self.exit_interrupt_error = None;
        self.cancellation = Some(CancellationTray {
            phase: CancellationPhase::Requesting,
            session_id,
            cluster,
            editor_restored,
        });
    }

    /// `Accepted`/`AlreadyInterrupted` fence live output at `cancel_seq` and
    /// settle the prompt exactly as a durable replay of the same `Cancel`
    /// entry would (see `fence_live_events_from_history`); `Idle` means
    /// there was nothing to interrupt, so only the tray clears.
    ///
    /// A child interrupt's tray never matches `active_remote_session` (it
    /// names the child's own storage key), so it only ever clears the tray
    /// here — the child's own monitor fences its own live state independently
    /// once durable history shows the `Cancel` (`fence_live_events_from_history`
    /// in `subagent_monitor.rs`), and its status converges through ordinary
    /// progress events. Fencing or settling here on a child's outcome would
    /// apply a foreign session's cancel sequence to this session's live state
    /// and tear down a parent turn the child's interrupt was never about.
    pub(crate) fn monitor_interrupt(&mut self, outcome: InterruptOutcome) {
        let targets_active_session = self.cancellation.as_ref().is_some_and(|tray| {
            self.active_remote_session.as_ref()
                == Some(&(tray.session_id.clone(), tray.cluster.clone()))
        });
        if targets_active_session {
            if let Some(cancel_seq) = outcome.cancel_seq() {
                self.live_events.accept_interrupt(cancel_seq);
                self.settle_interrupted_prompt();
            }
        }
        self.cancellation = None;
    }

    pub(super) fn settle_interrupted_prompt(&mut self) {
        self.retire_prompt_task();
        self.clear_tool_confirmation_route();
        // Set llm_busy = false BEFORE cancel_tool_confirm() so that
        // cancel_tool_confirm() never sees llm_busy == true and doesn't
        // emit a transient Working.
        self.app.llm_busy = false;
        self.cancel_tool_confirm();
        // Emit terminal status: prompt interrupted.
        crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Interrupted);
        self.app.pending_message = None;
        // Old readers retain their own queue, never G2's pending message slot.
        self.shared_pending_message = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        self.active_remote_session = None;
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.last_ui_output_source = None;
        self.flush_pending_thought();
        self.refresh_input_chrome();
    }

    pub(crate) fn handle_cancellation_key(&mut self, key: KeyEvent) -> bool {
        if self.cancellation.is_none() || self.app.modal.is_some() {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.app.should_quit = true;
                true
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                let tray = self.cancellation.as_ref().unwrap();
                if matches!(tray.phase, CancellationPhase::Failed(_)) {
                    let session_id = tray.session_id.clone();
                    let cluster = tray.cluster.clone();
                    self.start_cancellation(session_id, cluster);
                }
                true
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                self.cancellation.as_mut().unwrap().editor_restored = true;
                true
            }
            _ => {
                let editor_restored = self
                    .cancellation
                    .as_ref()
                    .is_some_and(|tray| tray.editor_restored);
                !editor_restored
            }
        }
    }

    pub(crate) fn render_cancellation_tray(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(tray) = &self.cancellation else {
            return;
        };
        let message = self.cancellation_message(tray, false);
        frame.render_widget(
            Paragraph::new(message).wrap(Wrap { trim: true }).block(
                Block::default()
                    .title("Cancellation")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            area,
        );
    }

    pub(crate) fn render_compact_cancellation_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(tray) = &self.cancellation else {
            return;
        };
        let message = format!("  {}", self.cancellation_message(tray, true));
        let status = Paragraph::new(Line::from(Span::styled(
            message,
            Style::default().fg(Color::Yellow),
        )));
        frame.render_widget(status, area);
    }

    pub(crate) fn cancellation_message(&self, tray: &CancellationTray, compact: bool) -> String {
        if compact {
            compact_cancellation_message(&tray.phase)
        } else {
            full_cancellation_message(&tray.phase)
        }
    }
}

/// The compact status renders as one unwrapped line, unlike the full tray,
/// which wraps inside its own box. An error long enough to fill it would
/// push the trailing `Ctrl+C: retry` hint past the terminal's edge, so the
/// error is truncated to keep the hint on screen — the mandated shape
/// (`"Interrupt failed: {error}  Ctrl+C: retry"`) is unchanged for any error
/// short enough to need none.
const COMPACT_ERROR_MAX_CHARS: usize = 40;

fn truncate_for_compact_tray(error: &str) -> String {
    if error.chars().count() <= COMPACT_ERROR_MAX_CHARS {
        return error.to_string();
    }
    let truncated: String = error.chars().take(COMPACT_ERROR_MAX_CHARS).collect();
    format!("{truncated}…")
}

fn compact_cancellation_message(phase: &CancellationPhase) -> String {
    match phase {
        CancellationPhase::Requesting => "Interrupting…".into(),
        CancellationPhase::Failed(error) => {
            format!(
                "Interrupt failed: {}  Ctrl+C: retry",
                truncate_for_compact_tray(error)
            )
        }
    }
}

fn full_cancellation_message(phase: &CancellationPhase) -> String {
    match phase {
        CancellationPhase::Requesting => {
            "Interrupting…  Esc: back to editor  Ctrl+D: exit immediately".into()
        }
        CancellationPhase::Failed(error) => {
            format!("Interrupt failed: {error}  Ctrl+C: retry  Ctrl+D: exit anyway  Esc: back")
        }
    }
}
