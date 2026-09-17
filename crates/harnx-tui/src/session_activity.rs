use crate::event_isolation::{EventStamp, SessionObservation};
use crate::types::{ModalState, Tui, TuiEvent};
use harnx_core::event::{AgentEvent, SessionEvent, TurnEvent};

use harnx_runtime::config::LOCAL_CLUSTER_KEY;

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
        terminal.draw(|frame| self.draw(frame))?;
        Ok(())
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
        if observation.historical
            && !matches!(event, AgentEvent::Turn(TurnEvent::SubAgentProgress(_)))
        {
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

    async fn refresh_shared_session_transcript(&mut self, stamp: &EventStamp) {
        let transcript = crate::lifecycle::session_history_transcript_items(&self.config).await;
        if !stamp.allows(&self.live_events) {
            return;
        }
        self.app.transcript = transcript;
        self.app.streaming_open = false;
        self.app.main_streamed_text_idx = None;
        self.app.last_ui_output_source = None;
        self.app.transcript_focus = None;
        self.app.transcript_selection_anchor = None;
        self.subagent_rows_dirty = true;
        self.pin_transcript_to_bottom();
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
                self.app.streaming_open = false;
                self.app.main_streamed_text_idx = None;
                self.refresh_input_chrome();
                true
            }
            AgentEvent::Turn(TurnEvent::Ended { .. } | TurnEvent::Interrupted { .. }) => {
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

    fn stop_session_activity_monitor(&mut self) {
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
            .unwrap_or_else(|| LOCAL_CLUSTER_KEY.to_string());
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
        let (selected, origin_agent, origin_session) = {
            let Some(ModalState::SessionPicker {
                selected,
                origin_agent,
                origin_session,
                ..
            }) = &self.app.modal
            else {
                return;
            };
            (*selected, origin_agent.clone(), origin_session.clone())
        };
        let (sessions, fetch_error) = Self::picker_sessions(&self.config).await;
        self.app.modal = Some(ModalState::SessionPicker {
            sessions,
            selected,
            origin_agent,
            origin_session,
            error: fetch_error,
        });
    }

    pub(super) async fn handle_refresh_session_list(&mut self) {
        let modal = self.app.modal.take();
        let Some(crate::types::ModalState::SessionPicker {
            selected,
            origin_agent,
            origin_session,
            ..
        }) = modal
        else {
            return;
        };

        let (sessions, fetch_error) = Self::picker_sessions(&self.config).await;
        self.app.modal = Some(crate::types::ModalState::SessionPicker {
            sessions,
            selected,
            origin_agent,
            origin_session,
            error: fetch_error,
        });
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.clear_tool_confirmation_route();
        self.stop_session_activity_monitor();
        self.stop_subagent_monitors();
    }
}
