//! Post-loop settlement and terminal outcome construction.
use super::{FollowBoundaryOptions, FollowLoopOutcome, FollowResult, FollowTarget};
use crate::nats_event_sink::{LiveEventState, SessionEventStream};
use crate::nats_session::NatsSession;
use anyhow::Result;
use harnx_core::event::{AgentEvent, AgentEventSink, ModelEvent};
use std::sync::Arc;

pub(super) struct FollowSettleContext<'a> {
    pub target: &'a FollowTarget<'a>,
    pub boundary_options: Option<&'a FollowBoundaryOptions>,
    pub event_sink: Arc<dyn AgentEventSink>,
    pub live: LiveEventState,
}

impl NatsSession {
    pub(super) async fn settle_follow_stream(
        &self,
        cx: FollowSettleContext<'_>,
        mut event_stream: SessionEventStream,
        outcome: FollowLoopOutcome,
    ) -> Result<FollowResult> {
        if let Some(res) = self
            .settle_cancellation_boundary(&cx, outcome.was_cancelled, outcome.cancel_confirmed)
            .await?
        {
            return Ok(res);
        }
        let (mut outcome, reload_err) = self.reload_final_durable_entries(&cx, outcome).await?;
        self.drain_and_flush_final_advisories(&mut event_stream, &cx, &mut outcome);
        self.build_final_follow_result(&cx, outcome, reload_err)
    }

    async fn settle_cancellation_boundary(
        &self,
        cx: &FollowSettleContext<'_>,
        was_cancelled: bool,
        cancel_confirmed: bool,
    ) -> Result<Option<FollowResult>> {
        if !was_cancelled {
            return Ok(None);
        }
        if let Some(boundary_opts) = cx.boundary_options.filter(|_| !cancel_confirmed) {
            let entries =
                crate::nats_session::completion::bounded_refresh(self.load_durable_entries())
                    .await?;
            if let Some(terminal) = self
                .observe_follow_boundary(&entries, cx.target, boundary_opts)?
                .filter(|boundary| boundary.pending.is_none())
            {
                return Ok(Some(terminal));
            }
            anyhow::bail!("cancellation has no durable follow boundary");
        }
        Ok(Some(
            crate::nats_session::NatsTurnResult {
                response: None,
                session_id: self.session_id.clone(),
                was_cancelled: true,
                error: None,
                user_msg_seq: cx.target.prompt_seq,
                user_msg_id: cx.target.prompt_id.to_string(),
            }
            .into(),
        ))
    }

    async fn reload_final_durable_entries(
        &self,
        cx: &FollowSettleContext<'_>,
        mut outcome: FollowLoopOutcome,
    ) -> Result<(FollowLoopOutcome, Option<anyhow::Error>)> {
        let mut final_reload_error = outcome.completion_error.take();
        if !outcome.turn_complete && final_reload_error.is_none() {
            match crate::nats_session::completion::bounded_refresh(self.load_durable_entries())
                .await
            {
                Ok(entries) => {
                    if let Some(boundary_opts) = cx.boundary_options {
                        outcome.observed_boundary =
                            self.observe_follow_boundary(&entries, cx.target, boundary_opts)?;
                    }
                    let (resp, err) = Self::extract_turn_outcome(&entries, cx.target.prompt_seq);
                    outcome.final_response = resp;
                    outcome.turn_error = err;
                    if let Ok(effective) =
                        harnx_core::session_reconstruct::apply_log_mutations_nats(&entries)
                    {
                        outcome.cached_effective = Some(effective);
                        crate::nats_session::emit_all_logical_seqs_for_window(
                            outcome.cached_effective.as_deref(),
                            &cx.event_sink,
                            &mut outcome.emitted_logical_seqs,
                        );
                    }
                }
                Err(error) => final_reload_error = Some(error),
            }
        }
        Ok((outcome, final_reload_error))
    }

    fn drain_and_flush_final_advisories(
        &self,
        stream: &mut SessionEventStream,
        cx: &FollowSettleContext<'_>,
        outcome: &mut FollowLoopOutcome,
    ) {
        while let Some(envelope) = stream.try_next() {
            if stream.should_render(&envelope) {
                outcome.saw_terminal_model_error |=
                    matches!(envelope.event, AgentEvent::Model(ModelEvent::Error(_)));
                outcome.pending_advisories.push_back(envelope);
            }
        }
        crate::nats_session::flush_pending_advisories(
            &mut outcome.pending_advisories,
            outcome.cached_effective.as_deref(),
            &cx.event_sink,
            &mut outcome.emitted_logical_seqs,
            crate::nats_session::AdvisoryFlush::Final,
            &cx.live,
            stream.last_applied_seq(),
        );
    }

    fn build_final_follow_result(
        &self,
        cx: &FollowSettleContext<'_>,
        outcome: FollowLoopOutcome,
        final_reload_error: Option<anyhow::Error>,
    ) -> Result<FollowResult> {
        if let Some(error) = final_reload_error {
            return Err(error);
        }
        if !outcome.saw_terminal_model_error {
            if let Some(error) = &outcome.turn_error {
                crate::nats_session::render_error_entry(error, &cx.event_sink);
            }
        }
        if let Some(observed) = outcome.observed_boundary {
            return Ok(observed);
        }
        if cx.boundary_options.is_some() {
            anyhow::bail!("event stream closed without a durable follow boundary");
        }
        Ok(crate::nats_session::NatsTurnResult {
            response: outcome.final_response,
            session_id: self.session_id.clone(),
            was_cancelled: outcome.was_cancelled || self.abort_signal.aborted(),
            error: outcome.turn_error,
            user_msg_seq: cx.target.prompt_seq,
            user_msg_id: cx.target.prompt_id.to_string(),
        }
        .into())
    }
}
