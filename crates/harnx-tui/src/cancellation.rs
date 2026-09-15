//! Cancellation is operational state, independent of transcript completion.
use crate::types::{CancellationAction, Tui};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_execution_control::{CancelDisposition, CancelReceipt};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use tokio::sync::mpsc;

mod hydration;

pub(crate) enum CancellationPhase {
    Requesting,
    Abandoning,
    Unconfirmed,
    Failed(String),
}

pub(crate) struct CancellationTray {
    pub phase: CancellationPhase,
    pub(crate) session_id: String,
    pub(crate) cluster: String,
    pub(crate) expected: Option<String>,
    pub execution_id: Option<String>,
    pub editor_restored: bool,
}

struct CancellationTarget {
    session_id: String,
    cluster: String,
    expected: Option<String>,
    execution_id: Option<String>,
}

impl Tui {
    pub(crate) fn cancellation_unconfirmed(&self) -> bool {
        self.cancellation.as_ref().is_some_and(|tray| {
            tray.expected.is_none()
                && matches!(
                    tray.phase,
                    CancellationPhase::Unconfirmed | CancellationPhase::Failed(_)
                )
        })
    }

    pub(crate) fn has_root_cancellation(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|tray| tray.expected.is_none())
    }

    pub(crate) fn cancellation_editor_restored(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(|tray| tray.expected.is_none() && tray.editor_restored)
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

    pub(super) fn start_observed_cancellation(
        &mut self,
        session_id: String,
        cluster: String,
        execution_id: String,
    ) {
        self.start_cancellation_for_generation(CancellationTarget {
            session_id,
            cluster,
            expected: None,
            execution_id: Some(execution_id),
        });
    }

    pub(crate) fn start_cancellation(
        &mut self,
        session_id: String,
        cluster: String,
        expected: Option<String>,
    ) {
        let execution_id = expected.clone().or_else(|| self.live_events.active());
        self.start_cancellation_for_generation(CancellationTarget {
            session_id,
            cluster,
            expected,
            execution_id,
        });
    }

    fn start_cancellation_for_generation(&mut self, target: CancellationTarget) {
        let CancellationTarget {
            session_id,
            cluster,
            expected,
            execution_id,
        } = target;
        let editor_restored = self
            .cancellation
            .as_ref()
            .is_some_and(|tray| tray.editor_restored);
        // Retries keep their original execution ID; restoring the editor is not
        // permission to target whichever generation is live now.

        self.pending_exit_cancel = Some((self.exit_cancel_factory)(
            self.config.clone(),
            self.local_worker.clone(),
            session_id.clone(),
            cluster.clone(),
            execution_id.clone(),
            CancellationAction::Request,
        ));
        self.exit_interrupt_error = None;
        self.cancellation = Some(CancellationTray {
            phase: CancellationPhase::Requesting,
            session_id,
            cluster,
            execution_id,
            expected,
            editor_restored,
        });
        if let Some(abort) = &self.current_prompt_abort {
            if self.cancellation.as_ref().is_some_and(|tray| {
                tray.expected.is_none() && tray.execution_id == self.live_events.active()
            }) {
                abort.set_ctrlc();
            }
        }
    }

    pub(crate) fn start_cancellation_abandonment(&mut self) {
        let Some(tray) = self.cancellation.as_ref() else {
            return;
        };
        if !matches!(
            tray.phase,
            CancellationPhase::Unconfirmed | CancellationPhase::Failed(_)
        ) {
            return;
        }
        let session_id = tray.session_id.clone();
        let cluster = tray.cluster.clone();
        let Some(expected) = tray.execution_id.clone() else {
            return;
        };
        self.pending_exit_cancel = Some((self.exit_cancel_factory)(
            self.config.clone(),
            self.local_worker.clone(),
            session_id,
            cluster,
            Some(expected),
            CancellationAction::Abandon,
        ));
        self.exit_interrupt_error = None;
        if let Some(tray) = self.cancellation.as_mut() {
            tray.phase = CancellationPhase::Abandoning;
        }
    }

    pub(crate) fn monitor_cancellation(&mut self, receipt: CancelReceipt) {
        self.live_events.accept_stop(&receipt);
        let Some(tray) = self.cancellation.as_ref() else {
            return;
        };
        if receipt.cancelled || receipt.disposition == CancelDisposition::Idle {
            // A child receipt or a delayed G1 receipt cannot retire G2's composer.
            let root = tray.expected.is_none();
            let matches = self.live_events.active().is_none()
                || self.live_events.active() == receipt.execution_id;
            if root && matches {
                self.settle_interrupted_prompt();
            }
            self.cancellation = None;
        } else if let Some(tray) = self.cancellation.as_mut() {
            tray.execution_id = receipt.execution_id;
            tray.phase = CancellationPhase::Unconfirmed;
        }
    }

    pub(super) fn settle_interrupted_prompt(&mut self) {
        self.retire_prompt_task();
        self.clear_tool_confirmation_route();
        self.resolve_tool_confirm(false);
        self.app.llm_busy = false;
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
        if !self.has_root_cancellation() || self.app.modal.is_some() {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.app.should_quit = true;
                true
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                let tray = self.cancellation.as_ref().unwrap();
                if matches!(
                    tray.phase,
                    CancellationPhase::Unconfirmed | CancellationPhase::Failed(_)
                ) {
                    self.start_cancellation_for_generation(CancellationTarget {
                        session_id: tray.session_id.clone(),
                        cluster: tray.cluster.clone(),
                        expected: tray.expected.clone(),
                        execution_id: tray.execution_id.clone(),
                    });
                }
                true
            }
            (KeyCode::Esc, KeyModifiers::NONE) => {
                let tray = self.cancellation.as_mut().unwrap();
                tray.editor_restored = true;
                if tray.execution_id.is_some()
                    && matches!(
                        tray.phase,
                        CancellationPhase::Unconfirmed | CancellationPhase::Failed(_)
                    )
                {
                    self.start_cancellation_abandonment();
                }
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
            compact_cancellation_message(&tray.phase, tray.execution_id.is_some())
        } else {
            full_cancellation_message(&tray.phase, tray.execution_id.is_some())
        }
    }
}

fn compact_cancellation_message(phase: &CancellationPhase, has_execution_id: bool) -> String {
    match (phase, has_execution_id) {
        (CancellationPhase::Requesting, _) => {
            "Cancellation unresolved; draft retained. Requesting…".into()
        }
        (CancellationPhase::Abandoning, _) => {
            "Resuming… Prior work may still run. Draft retained.".into()
        }
        (CancellationPhase::Unconfirmed, true) => {
            "Unconfirmed; draft retained. Work may run.  Ctrl+C: retry  Esc: resume anyway".into()
        }
        (CancellationPhase::Unconfirmed, false) => {
            "Cancellation unresolved; draft retained. Ctrl+C: retry.".into()
        }
        (CancellationPhase::Failed(error), true) => {
            format!("Ctrl+C: retry  Esc: resume anyway  —  Failed: {error}. Draft retained.")
        }
        (CancellationPhase::Failed(error), false) => {
            format!("Ctrl+C: retry  —  Failed: {error}. Draft retained.")
        }
    }
}

fn full_cancellation_message(phase: &CancellationPhase, has_execution_id: bool) -> String {
    match (phase, has_execution_id) {
        (CancellationPhase::Requesting, _) => {
            "Requesting cancellation…  Esc: back to editor  Ctrl+D: exit immediately".into()
        }
        (CancellationPhase::Abandoning, _) => {
            "Resuming with a new execution…  Prior work may still be running.  Esc: back to editor  Ctrl+D: exit".into()
        }
        (CancellationPhase::Unconfirmed, true) => {
            "Cancellation unconfirmed. Prior work may still be running.  Ctrl+C: retry  Esc: resume anyway  Ctrl+D: exit".into()
        }
        (CancellationPhase::Unconfirmed, false) => {
            "Cancellation unconfirmed. Prior work may still be running.  Ctrl+C: retry  Esc: back to editor  Ctrl+D: exit".into()
        }
        (CancellationPhase::Failed(error), true) => {
            format!("Cancellation request failed: {error}  Prior work may still be running.  Ctrl+C: retry  Esc: resume anyway  Ctrl+D: exit")
        }
        (CancellationPhase::Failed(error), false) => {
            format!("Cancellation request failed: {error}  Prior work may still be running.  Ctrl+C: retry  Esc: back to editor  Ctrl+D: exit")
        }
    }
}

pub(crate) async fn monitor_execution(
    config: &harnx_runtime::config::GlobalConfig,
    events: &mpsc::UnboundedSender<crate::types::TuiEvent>,
    target: &(String, String),
) {
    while !events.is_closed() {
        let snapshot = config.read().clone();
        let read = async {
            let js = snapshot.nats_jetstream(&target.1).await?;
            let bucket = js.get_key_value(harnx_execution_control::BUCKET).await?;
            let store = harnx_execution_control::ExecutionStore::from_store(bucket);
            let Some(mut operation) = store.current(&target.0).await? else {
                return Ok::<_, anyhow::Error>(());
            };
            if let Some(stop) = store.accepted_stop(&operation.reference).await? {
                // UI snapshot only. Cleanup convergence is not logical activity.
                operation.stop_decision = Some(stop.decision);
            } else {
                operation = store.status(&operation.reference).await?;
            }
            let _ = events.send(crate::types::TuiEvent::ExecutionState {
                cluster: target.1.clone(),
                operation,
            });
            Ok(())
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), read).await;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}
