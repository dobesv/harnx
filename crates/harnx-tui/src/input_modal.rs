//! Handle modal key input, picker selection and confirmation actions.

use crate::tool_confirmation::{ConfirmDecision, TOOL_CONFIRM_IDLE_GATE};
use crate::types::{ModalState, Tui};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_runtime::config::ConfigLock;

// Escape differs between startup and a picker opened from an existing session.
enum PickerCancelAction {
    RestoreOrigin,
    BackToAgents,
    Exit,
    Dismiss,
}

impl Tui {
    // qual:allow(iosp) reason: "the agent/session bootstrap branches share the one picker-opening contract; extraction would split a single interaction"
    pub(super) async fn check_picker_modal(&mut self) {
        let (no_agent, no_session) = {
            let cfg = self.config.read();
            (cfg.agent.is_none(), cfg.session.is_none())
        };
        if no_agent {
            // Agent discovery is a local config read; no broker wait.
            let agents = Self::assistant_agents_for_display(&self.config).await;
            if agents.is_empty() {
                return;
            }
            self.app.modal = Some(crate::types::ModalState::AgentPicker {
                agents,
                selected: 0,
                query: String::new(),
            });
        } else if no_session {
            self.open_session_picker();
        }
    }

    pub(crate) async fn open_agent_picker(&mut self) {
        let agents = Self::assistant_agents_for_display(&self.config).await;
        self.app.modal = Some(crate::types::ModalState::AgentPicker {
            agents,
            selected: 0,
            query: String::new(),
        });
    }

    /// Open the session picker immediately and load its list in the
    /// background: listing sessions can wait on managed-local acquisition,
    /// so opening (or refreshing) the picker must not block the input loop
    /// while a broker recovers (design 9). The list arrives as a
    /// `PickerSessionsLoaded` event and fills the open picker.
    pub(crate) fn open_session_picker(&mut self) {
        let origin_agent = self.config.read().active_agent_ref();
        let origin_session = self
            .config
            .read()
            .session
            .as_ref()
            .map(|s| s.id().to_string());
        self.open_session_picker_with_origin(origin_agent, origin_session);
    }

    /// Same picker-opening path with explicit Esc-restore origins: callers
    /// that just switched the agent capture the pre-switch values.
    pub(crate) fn open_session_picker_with_origin(
        &mut self,
        origin_agent: Option<String>,
        origin_session: Option<String>,
    ) {
        self.retire_picker_refresh();
        self.cancel_inflight_picker_unread_toggle();
        self.app.modal = Some(crate::types::ModalState::SessionPicker {
            sessions: vec![],
            selected: 0,
            origin_agent,
            origin_session,
            error: Some("Loading NATS sessions…".to_string()),
        });
        self.spawn_picker_sessions_refresh();
    }

    pub(super) async fn maybe_open_picker_after_command(
        &mut self,
        outcome: harnx_runtime::commands::CommandOutcome,
        prev_agent: Option<String>,
    ) {
        match outcome {
            harnx_runtime::commands::CommandOutcome::Continue => {
                let (curr_agent, session_missing) = {
                    let cfg = self.config.read();
                    (cfg.active_agent_ref(), cfg.session.is_none())
                };
                if prev_agent != curr_agent && session_missing {
                    self.open_session_picker();
                }
            }
            harnx_runtime::commands::CommandOutcome::Exit => {
                self.request_exit().await;
            }
            harnx_runtime::commands::CommandOutcome::OpenAgentPicker => {
                self.open_agent_picker().await;
            }
            harnx_runtime::commands::CommandOutcome::OpenSessionPicker => {
                self.open_session_picker();
            }
        }
    }

    async fn restore_picker_origin(&mut self) {
        let Some(crate::types::ModalState::SessionPicker {
            origin_agent: Some(agent),
            origin_session,
            ..
        }) = self.app.modal.as_ref()
        else {
            return;
        };
        let (agent, session) = (agent.clone(), origin_session.clone());
        // Prepare on a separate config so a missing origin agent cannot destroy
        // the current selection. Picker switching runs while the prompt is idle.
        let candidate = std::sync::Arc::new(ConfigLock::new(self.config.read().clone()));
        match harnx_runtime::config::Config::use_agent(
            &candidate,
            &agent,
            session.as_deref(),
            self.abort_signal.clone(),
        )
        .await
        {
            Ok(()) => {
                self.config
                    .write()
                    .apply_prepared_agent_selection(candidate.read().clone());
                self.retire_pending_title_rename();
                self.retire_picker_refresh();
                self.retire_info_overlay();
                self.app.modal = None;
                self.refresh_input_chrome();
            }
            Err(error) => {
                if let Some(crate::types::ModalState::SessionPicker { error: message, .. }) =
                    self.app.modal.as_mut()
                {
                    *message = Some(error.to_string());
                }
            }
        }
    }

    async fn handle_tool_confirmation_key(&mut self, key: KeyEvent) {
        let now = std::time::Instant::now();
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::NONE) => self.submit_confirmation_key(now, true).await,
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.submit_confirmation_key(now, false).await
            }
            (KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.edit_tool_confirmation(now, |state| state.message.insert_newline());
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.edit_tool_confirmation(now, |_| {});
                self.cancel_tool_confirm();
                self.handle_ctrl_c().await;
            }
            (KeyCode::Char('f'), KeyModifiers::CONTROL) => self.toggle_confirmation_view(now),
            (KeyCode::PageUp, KeyModifiers::NONE) => self.edit_tool_confirmation(now, |state| {
                for _ in 0..10 {
                    state.scroll.scroll_up();
                }
            }),
            (KeyCode::PageDown, KeyModifiers::NONE) => self.edit_tool_confirmation(now, |state| {
                for _ in 0..10 {
                    state.scroll.scroll_down();
                }
            }),
            _ => self.edit_tool_confirmation(now, |state| {
                state.message.input(key);
            }),
        }
    }

    async fn submit_confirmation_key(&mut self, now: std::time::Instant, approve: bool) {
        let Some(ModalState::ConfirmToolUse(state)) = self.app.modal.as_mut() else {
            return;
        };
        // Enter is idle-gated; Ctrl+D rejects immediately. Swallow early Enter
        // without resetting the idle deadline, including repeated keypresses.
        if approve && now.saturating_duration_since(state.last_key_at) < TOOL_CONFIRM_IDLE_GATE {
            return;
        }
        state.last_key_at = now;
        if state.submitting {
            return;
        }
        let decision = if approve {
            ConfirmDecision::Approve
        } else {
            ConfirmDecision::RejectToAgent
        };
        self.submit_tool_confirm(decision).await;
    }

    fn edit_tool_confirmation(
        &mut self,
        now: std::time::Instant,
        edit: impl FnOnce(&mut crate::types::ConfirmToolUseState),
    ) {
        if let Some(ModalState::ConfirmToolUse(state)) = self.app.modal.as_mut() {
            state.last_key_at = now;
            edit(state);
        }
    }

    fn toggle_confirmation_view(&mut self, now: std::time::Instant) {
        self.edit_tool_confirmation(now, |state| {
            if state.has_template {
                state.view = match state.view {
                    crate::types::ConfirmView::Template => crate::types::ConfirmView::RawYaml,
                    crate::types::ConfirmView::RawYaml => crate::types::ConfirmView::Template,
                };
            }
        });
    }

    /// Handle keystrokes while a modal is open. Each specialized modal owns
    /// its key bindings; delete and rewind confirmations use the y/n fallback.
    pub(super) async fn handle_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        match self.app.modal.as_ref() {
            Some(crate::types::ModalState::AgentPicker { .. })
            | Some(crate::types::ModalState::SessionPicker { .. }) => {
                self.handle_picker_key(key).await?;
            }
            Some(crate::types::ModalState::ConfirmInteraction(_)) => {
                self.handle_interaction_key(key).await;
            }
            Some(crate::types::ModalState::SurfaceAction(_)) => {
                self.handle_surface_key(key);
            }
            Some(crate::types::ModalState::ConfirmToolUse(_)) => {
                self.handle_tool_confirmation_key(key).await;
            }
            Some(crate::types::ModalState::ConfirmExit { phase, .. }) => {
                self.handle_confirm_exit_key(*phase, key)
            }
            Some(_) => match (key.code, key.modifiers) {
                (KeyCode::Char('y'), KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::NONE) => {
                    // The command path carries the full TUI command future. Keep that
                    // state off Tokio's comparatively small Windows worker stack.
                    Box::pin(self.confirm_modal_action()).await?;
                }
                (KeyCode::Char('n'), KeyModifiers::NONE) | (KeyCode::Esc, KeyModifiers::NONE) => {
                    self.app.modal = None;
                }
                _ => {}
            },
            None => {}
        }
        Ok(())
    }

    async fn handle_picker_key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::Char('c' | 'd')) && key.modifiers == KeyModifiers::CONTROL {
            self.request_exit().await;
            return Ok(());
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, _) => self.move_picker_selection(true),
            (KeyCode::Down, _) => self.move_picker_selection(false),
            (KeyCode::Char('u' | 'U'), KeyModifiers::NONE | KeyModifiers::SHIFT)
                if matches!(self.app.modal, Some(ModalState::SessionPicker { .. })) =>
            {
                self.handle_picker_mark_unread_toggle().await;
            }
            (KeyCode::Enter, _) => self.accept_picker_selection().await?,
            (KeyCode::Esc, _) => self.cancel_picker_selection().await,
            (KeyCode::Char(_), KeyModifiers::NONE | KeyModifiers::SHIFT)
            | (KeyCode::Backspace, _) => self.edit_agent_picker_query(key.code),
            _ => {}
        }
        Ok(())
    }

    fn move_picker_selection(&mut self, up: bool) {
        let (selected, limit) = match self.app.modal.as_mut() {
            Some(ModalState::AgentPicker {
                selected,
                agents,
                query,
            }) => (
                selected,
                ModalState::filtered_agents(agents, query)
                    .len()
                    .saturating_sub(1),
            ),
            // Session row zero is "New session"; remaining rows are sessions.
            Some(ModalState::SessionPicker {
                selected, sessions, ..
            }) => (selected, sessions.len()),
            _ => return,
        };
        if up {
            *selected = selected.saturating_sub(1);
        } else if *selected < limit {
            *selected += 1;
        }
    }

    fn edit_agent_picker_query(&mut self, key: KeyCode) {
        let Some(ModalState::AgentPicker {
            query, selected, ..
        }) = self.app.modal.as_mut()
        else {
            return;
        };
        match key {
            KeyCode::Char(ch) => {
                query.push(ch);
                *selected = 0;
            }
            KeyCode::Backspace => {
                query.pop();
                *selected = 0;
            }
            _ => {}
        }
    }

    async fn accept_picker_selection(&mut self) -> Result<()> {
        match self.app.modal.take() {
            Some(modal @ ModalState::AgentPicker { .. }) => self.accept_agent_picker(modal).await?,
            Some(modal @ ModalState::SessionPicker { .. }) => {
                self.accept_session_picker(modal).await?
            }
            modal => self.app.modal = modal,
        }
        Ok(())
    }

    async fn accept_agent_picker(&mut self, modal: ModalState) -> Result<()> {
        let ModalState::AgentPicker {
            agents,
            selected,
            query,
        } = modal
        else {
            return Ok(());
        };
        let filtered = ModalState::filtered_agents(&agents, &query);
        let Some(agent_name) = filtered.get(selected).cloned() else {
            self.app.modal = Some(ModalState::AgentPicker {
                agents,
                selected,
                query,
            });
            return Ok(());
        };
        let prev_session = self
            .config
            .read()
            .session
            .as_ref()
            .map(|session| session.id().to_string());
        let prev_agent = self.config.read().active_agent_ref();
        if let Err(error) = self.config.write().use_agent_by_name(&agent_name) {
            self.app.modal = Some(ModalState::AgentPicker {
                agents,
                selected,
                query,
            });
            return Err(error);
        }
        self.retire_pending_title_rename();
        self.retire_info_overlay();
        // Session listing happens in the background (design 9): the picker
        // opens empty and fills from the `PickerSessionsLoaded` event. The
        // pre-switch origins keep Esc-restore semantics.
        self.open_session_picker_with_origin(prev_agent, prev_session);
        Ok(())
    }

    async fn accept_session_picker(&mut self, modal: ModalState) -> Result<()> {
        let ModalState::SessionPicker {
            sessions,
            selected,
            origin_agent,
            origin_session,
            ..
        } = modal
        else {
            return Ok(());
        };
        if selected == 0 {
            self.retire_picker_refresh();
            // Reserving the new session id can wait on managed-local
            // acquisition; it runs as cancellable background work and
            // completes as a `NewSessionReserved`/`...Failed` event.
            self.select_new_session(crate::lifecycle::NewSessionSelection {
                sessions,
                selected,
                origin_agent,
                origin_session,
            });
            return Ok(());
        }
        let Some(session) = sessions.get(selected - 1) else {
            self.app.modal = Some(ModalState::SessionPicker {
                sessions,
                selected,
                origin_agent,
                origin_session,
                error: None,
            });
            return Ok(());
        };
        let session_name = session.id.clone();
        let unread = session.unread;
        if let Err(error) = self.config.write().use_session(Some(&session_name)) {
            self.app.modal = Some(ModalState::SessionPicker {
                sessions,
                selected,
                origin_agent,
                origin_session,
                error: None,
            });
            return Err(error);
        }
        self.retire_pending_title_rename();
        self.retire_picker_refresh();
        self.app.current_session_unread = unread;
        let busy = self.app.llm_busy;
        let pending = self.app.pending_message.is_some();
        Self::refresh_input_chrome_from_state(&self.config, &mut self.app, busy, pending);
        // Reconcile against the origin of the full agent/session picker flow.
        self.reconcile_transcript_after_command(origin_session, origin_agent, ".session")
            .await;
        Ok(())
    }

    fn picker_cancel_action(&self) -> PickerCancelAction {
        match self.app.modal.as_ref() {
            Some(ModalState::SessionPicker {
                origin_session: Some(_),
                ..
            }) => PickerCancelAction::RestoreOrigin,
            Some(ModalState::SessionPicker {
                origin_agent: None, ..
            }) => PickerCancelAction::BackToAgents,
            Some(ModalState::SessionPicker { .. }) => PickerCancelAction::Exit,
            Some(ModalState::AgentPicker { .. })
                if self.config.read().active_agent_ref().is_none() =>
            {
                PickerCancelAction::Exit
            }
            _ => PickerCancelAction::Dismiss,
        }
    }

    async fn cancel_picker_selection(&mut self) {
        match self.picker_cancel_action() {
            PickerCancelAction::RestoreOrigin => self.restore_picker_origin().await,
            PickerCancelAction::BackToAgents => {
                self.retire_picker_refresh();
                self.open_agent_picker().await;
            }
            PickerCancelAction::Exit => self.request_exit().await,
            PickerCancelAction::Dismiss => {
                self.retire_picker_refresh();
                self.app.modal = None;
            }
        }
    }

    async fn handle_picker_mark_unread_toggle(&mut self) {
        // The metadata-store write can wait on managed-local acquisition;
        // the toggle must never block the input loop (design 9). The whole
        // action — jetstream lookup, bucket ensure, mark read/unread — runs
        // as cancellable background work; the picker re-lists only when the
        // toggle finishes and the picker is still the current view.
        let Some(ModalState::SessionPicker {
            sessions, selected, ..
        }) = &self.app.modal
        else {
            return;
        };
        let Some(session) = selected
            .checked_sub(1)
            .and_then(|index| sessions.get(index))
        else {
            return;
        };
        let (cluster, storage_key, mark_unread) = {
            let config = self.config.read();
            let cluster = config
                .remote_agent
                .as_ref()
                .map(|(_, cluster)| cluster.clone())
                .unwrap_or_else(|| config.default_cluster_key().to_string());
            (
                cluster,
                harnx_core::session_identity::session_key(
                    session.agent_name.as_deref(),
                    &session.id,
                ),
                !session.unread,
            )
        };
        self.spawn_picker_unread_toggle(cluster, storage_key, mark_unread);
    }

    /// Execute the action associated with the current modal and clear it.
    async fn confirm_modal_action(&mut self) -> Result<()> {
        // A turn can start between opening the confirmation and pressing
        // Enter (e.g. another client submits to the shared session). Re-check
        // and drop the mutation instead of racing the worker.
        if self.app.llm_busy
            && matches!(
                self.app.modal,
                Some(ModalState::ConfirmDelete { .. } | ModalState::ConfirmRewind { .. })
            )
        {
            self.app.modal = None;
            self.show_busy_notice();
            return Ok(());
        }
        let Some(modal) = self.app.modal.take() else {
            return Ok(());
        };
        match modal {
            ModalState::ConfirmDelete { from, to, target } => {
                if !self.validate_history_command_target(Some(&target)) {
                    self.finish_transcript_confirmation();
                    self.show_stale_transcript_notice();
                    return Ok(());
                }
                let command = if from == to {
                    format!(".delete message {from}")
                } else {
                    format!(".delete message {from}-{to}")
                };
                self.run_bound_history_command(&command, target).await?;
            }
            ModalState::ConfirmRewind {
                seq,
                user_text,
                target,
            } => {
                if !self.validate_history_command_target(Some(&target)) {
                    self.finish_transcript_confirmation();
                    self.show_stale_transcript_notice();
                    return Ok(());
                }
                self.run_bound_history_command(&format!(".rewind {seq}"), target)
                    .await?;
                if let Some(text) = user_text {
                    self.set_input_text(&text);
                }
            }
            _ => return Ok(()),
        }
        self.finish_transcript_confirmation();
        Ok(())
    }

    fn finish_transcript_confirmation(&mut self) {
        self.app.detail_view_open = false;
        self.app.detail_view_text = None;
        self.app.detail_view_title = None;
        self.app.transcript_browsing = false;
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
    }
}
