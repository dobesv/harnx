use crate::event_isolation::{EventStamp, SessionObservation};
use crate::types::{ModalState, Tui, TuiEvent};
use harnx_core::event::{AgentEvent, SessionEvent, TurnEvent};

mod monitor;
use monitor::spawn_session_activity_monitor;
pub(super) use monitor::{attach_session_event_stream_with_state, history_has_pending_turn};

impl Tui {
    pub(super) fn draw_with_session_activity<B>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> anyhow::Result<()>
    where
        B: ratatui::backend::Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        self.sync_session_activity_monitor();
        self.sync_interaction_client();
        self.sync_surface_client();
        terminal.draw(|frame| self.draw(frame))?;
        Ok(())
    }

    pub(super) fn handle_session_stream_attached(
        &mut self,
        observation: SessionObservation,
        _active: bool,
    ) {
        if !observation.accepts(self) {
            return;
        }
        let reconnected = self.session_stream_attached || self.session_attachment_gen == 0;
        if reconnected {
            self.session_attachment_gen = self.session_attachment_gen.saturating_add(1);
        }
        self.session_stream_attached = true;
        if self.is_history_repair_pending() && (reconnected || self.history_rebuild_abort.is_none())
        {
            self.schedule_history_repair();
        }
    }

    pub(super) async fn handle_session_activity(
        &mut self,
        observation: SessionObservation,
        active: bool,
    ) {
        if !observation.accepts(self) {
            return;
        }
        let target = observation.target.clone();
        if self.app.llm_busy == active {
            return;
        }
        if active {
            self.app.llm_busy = true;
            self.active_remote_session = Some(target.clone());
            self.refresh_input_chrome();
            // Emit terminal status: session activity started.
            crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Working);
            // Reopening a session can reveal a durable pending turn after its
            // prior worker vanished. Re-activation is idempotent under the
            // session lease and guarantees that "busy" has an owner that can
            // either resume successfully or write a terminal Error.
            #[cfg(not(test))]
            if let Err(error) = self
                .activate_pending_session(target.0.clone(), target.1.clone())
                .await
            {
                log::warn!(
                    "failed to reactivate pending attached session {}: {error:#}",
                    target.0
                );
            }
        } else {
            // A reconnect may miss the lossy TurnEnded advisory. If this TUI
            // previously observed the foreign turn, converge from its durable
            // history before clearing the busy state.
            if self.active_remote_session.as_ref() == Some(&target) {
                self.refresh_shared_session_transcript(&observation.stamp)
                    .await;
            }
            if observation.accepts(self) {
                self.complete_main_prompt().await;
            }
        }
    }

    pub(super) async fn handle_shared_session_agent_event(
        &mut self,
        observation: SessionObservation,
        event: AgentEvent,
    ) {
        if !observation.accepts(self) {
            return;
        }
        if observation.historical {
            if let AgentEvent::Turn(TurnEvent::SubAgentProgress(progress)) = event {
                self.record_recovered_subagent_progress(progress);
            }
            return;
        }
        let refresh_before = matches!(event, AgentEvent::Turn(TurnEvent::Started));
        let refresh_after = matches!(
            event,
            AgentEvent::Turn(TurnEvent::Ended { .. } | TurnEvent::Interrupted { .. })
        );
        if refresh_before {
            self.refresh_shared_session_transcript(&observation.stamp)
                .await;
        }
        if !observation.accepts(self) {
            return;
        }
        self.render_agent_event(event).await;
        if refresh_after {
            // Advisory fan-out is deliberately lossy. Rebuild at the durable
            // turn boundary so a missed final chunk still converges exactly to
            // what reopening the session would show.
            self.refresh_shared_session_transcript(&observation.stamp)
                .await;
        }
    }

    /// Rebuild the transcript from durable history. `pub(crate)` so the
    /// pending-interaction regressions can exercise the real refresh path
    /// (review B1/B2), not only the normalization helper.
    pub(crate) async fn refresh_shared_session_transcript(&mut self, stamp: &EventStamp) {
        self.schedule_shared_session_refresh(stamp);
    }

    pub(super) async fn handle_turn_activity(
        &mut self,
        event: &AgentEvent,
        is_sub_agent: bool,
    ) -> bool {
        if is_sub_agent {
            return false;
        }
        match event {
            AgentEvent::Turn(TurnEvent::Started) => {
                self.app.llm_busy = true;
                // Emit terminal status: turn started.
                crate::terminal_status::set_status(crate::terminal_status::TerminalStatus::Working);
                self.app.streaming_open = false;
                self.app.main_streamed_text_idx = None;
                self.app.streamed_text_idx = None;
                self.refresh_input_chrome();
                true
            }
            AgentEvent::Turn(TurnEvent::Ended { .. }) => {
                self.flush_pending_thought();
                self.app.streaming_open = false;
                self.complete_main_prompt().await;
                true
            }
            AgentEvent::Turn(TurnEvent::Interrupted { .. }) => {
                // Emit terminal status: turn interrupted.
                crate::terminal_status::set_status(
                    crate::terminal_status::TerminalStatus::Interrupted,
                );
                self.flush_pending_thought();
                self.app.streaming_open = false;
                self.complete_main_prompt().await;
                true
            }
            _ => false,
        }
    }

    pub(super) fn sync_session_activity_monitor(&mut self) {
        self.sync_subagent_monitor_root();
        self.sync_known_subagent_rows();
        let confirmation_target = self.session_activity_destination();
        self.sync_tool_confirmation_route(confirmation_target.as_ref());
        let desired = self
            .current_prompt_abort
            .is_none()
            .then(|| self.session_activity_destination())
            .flatten();
        if desired == self.session_activity_target {
            return;
        }

        self.stop_session_activity_monitor();
        self.session_attachment_gen = self.session_attachment_gen.saturating_add(1);
        self.session_stream_attached = false;
        self.session_activity_target = desired.clone();
        self.session_activity_handle = desired.map(|target| {
            self.live_events = self.live_events.fork();
            spawn_session_activity_monitor(
                self.config.clone(),
                self.event_tx.clone(),
                target,
                self.live_events.clone(),
            )
        });
    }

    pub(super) fn stop_session_activity_monitor(&mut self) {
        if let Some(handle) = self.session_activity_handle.take() {
            handle.abort();
        }
    }

    pub(super) fn session_activity_destination(&self) -> Option<(String, String)> {
        let config = self.config.read();
        let session_id = config.session.as_ref()?.storage_key();
        let cluster = config
            .remote_agent
            .as_ref()
            .map(|(_, cluster)| cluster.clone())
            .unwrap_or_else(|| config.default_cluster_key().to_string());
        Some((session_id, cluster))
    }

    /// Handle a read invalidation event for a session.
    /// Updates current-session chrome and any open session picker from canonical state.
    pub(super) async fn handle_session_read_invalidation(&mut self, session_id: &str) {
        self.refresh_current_session_read_state(session_id).await;
        self.refresh_session_picker_for_read_invalidation().await;
    }

    async fn refresh_current_session_read_state(&mut self, session_id: &str) {
        let Some((current_session_id, cluster)) = self.session_activity_destination() else {
            return;
        };
        if current_session_id != session_id {
            return;
        }
        let config = self.config.read().clone();
        let Ok(jetstream) = config.nats_jetstream(&cluster).await else {
            return;
        };
        let Ok(store) =
            harnx_runtime::nats_session_metadata::SessionMetadataStore::ensure(&jetstream, 1).await
        else {
            return;
        };
        let Ok(read_state) = store.get_read_state(session_id).await else {
            return;
        };
        self.app.current_session_unread = read_state.is_unread();
        self.refresh_input_chrome();
    }

    async fn refresh_session_picker_for_read_invalidation(&mut self) {
        if !matches!(self.app.modal, Some(ModalState::SessionPicker { .. })) {
            return;
        }
        // The re-list can wait on managed-local acquisition; run it as
        // cancellable background work (design 9). The completion updates the
        // still-open picker in place.
        self.spawn_picker_sessions_refresh();
    }

    pub(super) async fn handle_refresh_session_list(&mut self) {
        if !matches!(
            self.app.modal,
            Some(crate::types::ModalState::SessionPicker { .. })
        ) {
            return;
        }
        // Same background refresh as the read-invalidation path: the periodic
        // reconcile event never blocks the event loop on the broker.
        self.spawn_picker_sessions_refresh();
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.clear_tool_confirmation_route();
        self.stop_session_activity_monitor();
        self.stop_subagent_monitors();
        self.cancel_inflight_history_rebuild();
        self.abort_history_repair_retry();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_session_refresh_exits_browse_mode() {
        // The rebuilt transcript invalidates the browse selection; leaving
        // transcript_browsing set with no focus would strand the user in a
        // browse mode whose action keys can only refuse.
        let config = crate::tests::test_config();
        let mut tui = Tui::init(&config).await.unwrap();
        tui.app.transcript_browsing = true;
        tui.app.transcript_focus = Some(0);
        tui.app.transcript_selection_anchor = Some(0);
        let stamp = crate::event_isolation::EventStamp::live(&tui.live_events);

        tui.refresh_shared_session_transcript(&stamp).await;
        tui.flush_history_work_for_test().await;

        assert!(!tui.app.transcript_browsing);
        assert_eq!(tui.app.transcript_focus, None);
        assert_eq!(tui.app.transcript_selection_anchor, None);
    }
}
