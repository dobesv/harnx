//! Cancellation is operational state, independent of transcript completion.
use crate::types::{CancellationAction, ModalState, Tui};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_execution_control::{CancelDisposition, CancelReceipt};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use tokio::sync::mpsc;

pub(crate) enum CancellationPhase {
    Requesting,
    Abandoning,
    Stopping,
    Unconfirmed,
    Failed(String),
}

pub(crate) struct CancellationTray {
    pub phase: CancellationPhase,
    session_id: String,
    cluster: String,
    expected: Option<String>,
    execution_id: Option<String>,
    updates: Option<mpsc::UnboundedReceiver<CancelReceipt>>,
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

    pub(crate) fn hydrate_execution_state(
        &mut self,
        cluster: String,
        operation: harnx_execution_control::Operation,
    ) {
        use crate::types::{SubAgentStatus, TranscriptItem};
        use harnx_execution_control::OperationState;
        let status = match operation.state {
            OperationState::CancelRequested | OperationState::Quiescing => {
                SubAgentStatus::Cancelling
            }
            OperationState::Unconfirmed => SubAgentStatus::Unconfirmed,
            OperationState::Cancelled => SubAgentStatus::Cancelled,
            OperationState::Completed => SubAgentStatus::Completed,
            _ => SubAgentStatus::Running,
        };
        let update_rows = |items: &mut Vec<TranscriptItem>| {
            for item in items {
                if let TranscriptItem::SubAgentSession {
                    key,
                    invocation_id,
                    status: row_status,
                    ..
                } = item
                {
                    if key.session_id == operation.reference.session_id
                        && invocation_id.as_deref() == Some(&operation.reference.execution_id)
                        && (operation.state.cancelling()
                            || operation.state == OperationState::Cancelled)
                    {
                        *row_status = status.clone();
                    }
                }
            }
        };
        update_rows(&mut self.app.transcript);
        for (key, state) in &mut self.app.monitored_sessions {
            if key.session_id == operation.reference.session_id && key.cluster == cluster {
                state.execution_id = Some(operation.reference.execution_id.clone());
                state.status = status.clone();
            }
            update_rows(&mut state.transcript);
        }
        for view in &mut self.app.subagent_view_stack {
            if view.key.session_id == operation.reference.session_id
                && view.progress.as_ref().is_some_and(|progress| {
                    progress.snapshot.invocation_id == operation.reference.execution_id
                })
            {
                view.status = status.clone();
            }
        }
        if self.session_activity_target.as_ref()
            == Some(&(operation.reference.session_id.clone(), cluster.clone()))
            && operation.state.cancelling()
            && self.cancellation.is_none()
        {
            // Cancellation cannot safely be undone after any descendant may
            // already have stopped. Re-issue it on attachment so an abandoned
            // local execution is targeted at this frontend's replacement
            // worker instead of leaving the user in a passive dead end.
            self.start_cancellation(operation.reference.session_id.clone(), cluster, None);
        }
    }

    pub(crate) fn start_cancellation(
        &mut self,
        session_id: String,
        cluster: String,
        expected: Option<String>,
    ) {
        self.pending_exit_cancel = Some((self.exit_cancel_factory)(
            self.config.clone(),
            self.local_worker.clone(),
            session_id.clone(),
            cluster.clone(),
            expected.clone(),
            CancellationAction::Request,
        ));
        self.exit_interrupt_error = None;
        self.cancellation = Some(CancellationTray {
            phase: CancellationPhase::Requesting,
            session_id,
            cluster,
            execution_id: expected.clone(),
            expected,
            updates: None,
        });
        if let Some(abort) = &self.current_prompt_abort {
            if self
                .cancellation
                .as_ref()
                .is_some_and(|tray| tray.expected.is_none())
            {
                abort.set_ctrlc();
            }
        }
    }

    pub(crate) fn start_cancellation_abandonment(&mut self) {
        let Some(tray) = self.cancellation.as_ref() else {
            return;
        };
        if !matches!(tray.phase, CancellationPhase::Unconfirmed) {
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
        if matches!(
            receipt.disposition,
            CancelDisposition::Idle | CancelDisposition::Cancelled
        ) {
            self.cancellation = None;
            return;
        }
        let Some(tray) = self.cancellation.as_mut() else {
            return;
        };
        tray.execution_id = receipt.execution_id.clone();
        tray.phase = CancellationPhase::Stopping;
        let (tx, rx) = mpsc::unbounded_channel();
        tray.updates = Some(rx);
        let target = (tray.session_id.clone(), tray.cluster.clone());
        let config = self.config.clone();
        tokio::spawn(async move {
            let session = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::remote_session::cancellation_status_session_for_target(
                    &config, target.0, target.1,
                ),
            )
            .await;
            let Ok(Ok(session)) = session else {
                let mut status = receipt;
                status.disposition = CancelDisposition::Unconfirmed;
                let _ = tx.send(status);
                return;
            };
            loop {
                let status = match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    session.cancel_status(&receipt),
                )
                .await
                {
                    Ok(Ok(status)) => status,
                    _ => {
                        let mut status = receipt.clone();
                        status.disposition = CancelDisposition::Unconfirmed;
                        status
                    }
                };
                let terminal = matches!(
                    status.disposition,
                    CancelDisposition::Idle | CancelDisposition::Cancelled
                );
                if tx.send(status).is_err() || terminal {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
    }

    pub(crate) fn poll_cancellation_status(&mut self) {
        let Some(tray) = self.cancellation.as_mut() else {
            return;
        };
        let Some(updates) = tray.updates.as_mut() else {
            return;
        };
        while let Ok(status) = updates.try_recv() {
            match status.disposition {
                CancelDisposition::Cancelled | CancelDisposition::Idle => {
                    self.cancellation = None;
                    return;
                }
                CancelDisposition::Unconfirmed => tray.phase = CancellationPhase::Unconfirmed,
                _ => tray.phase = CancellationPhase::Stopping,
            }
        }
    }

    pub(crate) fn handle_cancellation_key(&mut self, key: KeyEvent) -> bool {
        if !self.has_root_cancellation() || self.app.modal.is_some() {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => self.app.should_quit = true,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                let tray = self.cancellation.as_ref().unwrap();
                if matches!(
                    tray.phase,
                    CancellationPhase::Unconfirmed | CancellationPhase::Failed(_)
                ) {
                    self.start_cancellation(
                        tray.session_id.clone(),
                        tray.cluster.clone(),
                        tray.expected.clone(),
                    );
                }
            }
            (KeyCode::Esc, KeyModifiers::NONE)
                if self
                    .cancellation
                    .as_ref()
                    .is_some_and(|tray| matches!(tray.phase, CancellationPhase::Unconfirmed)) =>
            {
                self.app.modal = Some(ModalState::ConfirmAbandonCancellation);
            }
            _ => {}
        }
        true
    }

    pub(crate) fn render_cancellation_tray(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(tray) = &self.cancellation else {
            return;
        };
        let message = match &tray.phase {
            CancellationPhase::Requesting => {
                "Requesting cancellation…  Ctrl+D: exit immediately".into()
            }
            CancellationPhase::Abandoning => {
                "Resuming with a new execution…  Prior work may still be running".into()
            }
            CancellationPhase::Stopping => {
                "Stopping…  Waiting for execution and child operations to stop.  Ctrl+D: exit"
                    .into()
            }
            CancellationPhase::Unconfirmed => {
                "Cancellation unconfirmed. Work may still be running.  Ctrl+C: retry  Esc: resume anyway  Ctrl+D: exit".into()
            }
            CancellationPhase::Failed(error) => {
                format!("Cancellation request failed: {error}  Ctrl+C: retry  Ctrl+D: exit")
            }
        };
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
            let Some(operation) = store.current(&target.0).await? else {
                return Ok::<_, anyhow::Error>(());
            };
            let operation = store.status(&operation.reference).await?;
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
