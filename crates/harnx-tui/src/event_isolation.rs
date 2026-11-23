//! The UI queue is another late-delivery boundary, not evidence of current ownership.
use crate::types::{TranscriptItem, Tui, TuiEvent};
use anyhow::Result;
use harnx_core::{abort::AbortSignal, event::AgentEvent};
use harnx_runtime::nats_event_sink::LiveEventState;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct EventStamp {
    pub state: LiveEventState,
}

impl EventStamp {
    /// Stamp captured when a live advisory is admitted into the frontend queue.
    pub fn live(state: &LiveEventState) -> Self {
        Self {
            state: state.clone(),
        }
    }

    /// Stamp captured for an event synthesized from durable/recovery state.
    /// Construction is identical to `live` — both readers race the same
    /// attachment fence — kept as a separate name for call-site provenance.
    pub fn snapshot(state: &LiveEventState) -> Self {
        Self {
            state: state.clone(),
        }
    }

    /// Whether the attachment this stamp was captured under is still the one
    /// `current` reads from. A retired or replaced attachment (a new prompt,
    /// a reattached monitor) never allows a stamp captured before it.
    pub fn allows(&self, current: &LiveEventState) -> bool {
        current.same_attachment(&self.state)
    }
}

pub(crate) struct SessionObservation {
    pub target: (String, String),
    pub stamp: EventStamp,
    pub historical: bool,
}

/// The observation record both session-event arms share: which session the
/// event targets, the attachment stamp it was captured under, and whether it
/// comes from durable history rather than the live stream.
fn session_observation(
    historical: bool,
    stamp: EventStamp,
    session_id: String,
    cluster: String,
) -> SessionObservation {
    SessionObservation {
        target: (session_id, cluster),
        stamp,
        historical,
    }
}

impl SessionObservation {
    pub fn accepts(&self, tui: &Tui) -> bool {
        tui.session_activity_target.as_ref() == Some(&self.target)
            && tui.current_prompt_abort.is_none()
            && self.stamp.allows(&tui.live_events)
    }
}

impl Tui {
    /// Type-erased seam for the run loop (PR #2835 Windows CI query-depth
    /// overflow): `Pin<Box<dyn Future>>` stops rustc's cumulative layout
    /// walk of the loop's future at this await; see AGENTS.md "Future size
    /// and type erasure in harnx-runtime".
    pub(crate) fn handle_tui_event_erased(
        &mut self,
        event: TuiEvent,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + '_>> {
        Box::pin(self.handle_tui_event_inner(event))
    }

    pub(crate) async fn handle_tui_event(&mut self, event: TuiEvent) -> Result<()> {
        self.handle_tui_event_erased(event).await
    }

    pub(super) async fn handle_tui_event_inner(&mut self, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Interaction(event) => self.handle_interaction_event(event),
            TuiEvent::Surface(event) => self.handle_surface_event(event),
            TuiEvent::CredentialsUpdate { task, text } => {
                self.handle_credentials_update(task, text);
            }
            event @ (TuiEvent::OperatorToolFinished { .. }
            | TuiEvent::PromptTaskFinished { .. }) => {
                self.handle_task_completion(event).await;
            }
            // Explicit local commands are not worker advisories.
            TuiEvent::LocalAgent(event) => self.render_agent_event(event).await,
            TuiEvent::Agent { task, stamp, event } => {
                self.handle_prompt_agent_event(task, stamp, event).await;
            }
            TuiEvent::SessionActivity {
                historical,
                stamp,
                session_id,
                cluster,
                active,
            } => {
                self.on_session_activity(historical, stamp, session_id, cluster, active)
                    .await;
            }
            TuiEvent::SessionAgent {
                stamp,
                historical,
                session_id,
                cluster,
                event,
            } => {
                self.on_shared_session_agent(historical, stamp, session_id, cluster, event)
                    .await;
            }
            TuiEvent::SubAgentSessionSnapshot { key, snapshot } => {
                self.handle_subagent_snapshot(key, snapshot);
            }
            TuiEvent::SubAgentSessionEvent { key, stamp, event } => {
                self.handle_subagent_session_event(key, stamp, event);
            }
            TuiEvent::SubAgentInvocationFailed { key, invocation_id } => {
                self.fail_monitored_invocation(&key, &invocation_id);
            }
            event @ (TuiEvent::ToolRoundComplete | TuiEvent::PendingMessageConsumed(_)) => {
                self.handle_pending_prompt_event(event).await;
            }
            event @ (TuiEvent::ToolConfirmation(_)
            | TuiEvent::ToolConfirmationEnqueueFinished { .. }) => {
                self.handle_confirmation_completion(event).await;
            }
            event @ (TuiEvent::SessionReadInvalidation { .. } | TuiEvent::RefreshSessionList) => {
                self.handle_session_refresh_event(event).await;
            }
        }
        Ok(())
    }

    /// Fold the activity arm's payload into the observation record its
    /// handler gates on; keeps the flat dispatch match under the line budget.
    async fn on_session_activity(
        &mut self,
        historical: bool,
        stamp: EventStamp,
        session_id: String,
        cluster: String,
        active: bool,
    ) {
        self.handle_session_activity(
            session_observation(historical, stamp, session_id, cluster),
            active,
        )
        .await;
    }

    /// Same fold for the shared-session agent arm.
    async fn on_shared_session_agent(
        &mut self,
        historical: bool,
        stamp: EventStamp,
        session_id: String,
        cluster: String,
        event: AgentEvent,
    ) {
        self.handle_shared_session_agent_event(
            session_observation(historical, stamp, session_id, cluster),
            event,
        )
        .await;
    }

    async fn handle_task_completion(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::OperatorToolFinished {
                task,
                output,
                error,
            } => {
                self.finish_operator_tool_command(task, output, error).await;
            }
            TuiEvent::PromptTaskFinished { task, error } => {
                self.finish_prompt_task(task, error).await;
            }
            _ => unreachable!("task completion dispatch received a different event"),
        }
    }

    async fn handle_confirmation_completion(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::ToolConfirmation(event) => {
                self.handle_tool_confirmation_event(event).await;
            }
            TuiEvent::ToolConfirmationEnqueueFinished {
                confirmation_id,
                decision,
                result,
            } => {
                self.finish_tool_confirmation_enqueue(confirmation_id, decision, result);
            }
            _ => unreachable!("confirmation dispatch received a different event"),
        }
    }

    async fn handle_session_refresh_event(&mut self, event: TuiEvent) {
        // These NATS reads need boxing to keep the shared dispatch frame small.
        match event {
            TuiEvent::SessionReadInvalidation { session_id } => {
                Box::pin(self.handle_session_read_invalidation(&session_id)).await;
            }
            TuiEvent::RefreshSessionList => {
                Box::pin(self.handle_refresh_session_list()).await;
            }
            _ => unreachable!("session refresh event"),
        }
    }
    async fn handle_pending_prompt_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::ToolRoundComplete => {
                // Intermediate tool round — prompt loop continues, don't clear llm_busy.
                // Flush any pending thought so follow-up thought after tool results
                // starts a fresh block instead of appending to the earlier one.
                self.flush_pending_thought();
                // Reset streaming index so the next LLM turn creates a fresh
                // AssistantText item instead of appending to the previous one.
                // This keeps tool-call rows visually between the two turns.
                self.close_assistant_stream();
                self.app.streaming_open = false;
                self.app.main_streamed_text_idx = None;
                self.app.streamed_text_idx = None;
                self.pin_transcript_to_bottom();
            }
            TuiEvent::PendingMessageConsumed(pending) => {
                // The prompt task consumed our pending message during a tool
                // round.  Clear the local pending state, reset the input field,
                // and show the consumed text (and any attachments) in the
                // transcript.
                self.app.pending_message = None;
                self.app.input = Self::new_input();
                self.app.transcript.push(TranscriptItem::UserText {
                    text: pending.text.clone(),
                    seq: None,
                    timestamp: Some(chrono::Utc::now()),
                });
                for link in crate::markdown_render::extract_markdown_links(&pending.text) {
                    self.app.transcript.push(TranscriptItem::MarkdownLink {
                        text: link.text,
                        url: link.url,
                    });
                }
                self.render_submitted_attachments(&pending.attachments)
                    .await;
                self.pin_transcript_to_bottom();
                self.refresh_input_chrome();
            }
            _ => unreachable!("pending prompt event"),
        }
    }

    pub(super) async fn handle_prompt_agent_event(
        &mut self,
        task: AbortSignal,
        stamp: EventStamp,
        event: AgentEvent,
    ) {
        if !self
            .current_prompt_abort
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &task))
            || !stamp.allows(&self.live_events)
        {
            return;
        }
        // One guard covers every reducer, including Final, errors, progress,
        // LogSeqAssigned and Turn::Ended, before any transcript or busy mutation.
        self.render_agent_event(event).await;
    }
}

#[cfg(test)]
pub(crate) mod tests;
