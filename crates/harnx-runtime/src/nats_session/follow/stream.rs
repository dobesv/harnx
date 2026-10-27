//! Select loop execution for follower streaming.
use super::{CompletionDurableEffect, FollowBoundaryOptions, FollowResult, FollowTarget};
use crate::nats_event_sink::{AdvisoryEnvelope, LiveEventState, SessionEventStream};
use crate::nats_session::NatsSession;
use anyhow::Result;
use futures_util::StreamExt;
use harnx_core::{
    abort::wait_abort_signal,
    event::{AgentEvent, AgentEventSink, ModelEvent},
    session::SessionLogEntry,
};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) struct FollowLoopParams<'a> {
    pub target: &'a FollowTarget<'a>,
    pub event_sink: &'a Arc<dyn AgentEventSink>,
    pub pending_cancel: Option<mpsc::Receiver<()>>,
    pub options: &'a crate::nats_session::RunTurnOptions,
    pub boundary_options: Option<&'a FollowBoundaryOptions>,
    pub initial_entries: Vec<(u64, SessionLogEntry)>,
    pub initial_boundary: Option<FollowResult>,
    pub live: &'a LiveEventState,
}

pub(super) struct FollowLoopOutcome {
    pub final_response: Option<String>,
    pub turn_error: Option<String>,
    pub turn_complete: bool,
    pub observed_boundary: Option<FollowResult>,
    pub was_cancelled: bool,
    pub cancel_confirmed: bool,
    pub saw_terminal_model_error: bool,
    pub completion_error: Option<anyhow::Error>,
    pub cached_effective: Option<Vec<(u64, SessionLogEntry)>>,
    pub pending_advisories: VecDeque<AdvisoryEnvelope>,
    pub emitted_logical_seqs: HashSet<usize>,
}

pub(super) struct FollowLoopContext<'a> {
    pub target: &'a FollowTarget<'a>,
    pub boundary_options: Option<&'a FollowBoundaryOptions>,
    pub event_sink: &'a Arc<dyn AgentEventSink>,
    pub live: &'a LiveEventState,
}

pub(super) struct FollowStreamState {
    pub cached_effective: Option<Vec<(u64, SessionLogEntry)>>,
    pub pending_advisories: VecDeque<AdvisoryEnvelope>,
    pub emitted_logical_seqs: HashSet<usize>,
    pub emitted_subagent_completions: HashSet<String>,
}

pub(super) struct FollowStreamDriver<'a> {
    pub stream: &'a mut SessionEventStream,
    pub state: &'a mut FollowStreamState,
}

struct FollowLoopRuntime {
    final_response: Option<String>,
    turn_error: Option<String>,
    turn_complete: bool,
    observed_boundary: Option<FollowResult>,
    was_cancelled: bool,
    cancel_confirmed: bool,
    saw_terminal_model_error: bool,
    completion_error: Option<anyhow::Error>,
}

struct LoopUpdateDriver<'a, 'b> {
    pub driver: FollowStreamDriver<'b>,
    pub rt: &'a mut FollowLoopRuntime,
}

struct FollowLoopSession<'a> {
    state: FollowStreamState,
    rt: FollowLoopRuntime,
    updates: std::pin::Pin<
        Box<
            dyn futures_util::Stream<Item = Result<crate::nats_session::completion::DurableUpdate>>
                + Send,
        >,
    >,
    pending_cancel: Option<mpsc::Receiver<()>>,
    cx: FollowLoopContext<'a>,
}

impl FollowLoopRuntime {
    fn new(observed_boundary: Option<FollowResult>) -> Self {
        Self {
            final_response: None,
            turn_error: None,
            turn_complete: false,
            observed_boundary,
            was_cancelled: false,
            cancel_confirmed: false,
            saw_terminal_model_error: false,
            completion_error: None,
        }
    }

    fn check_boundary_complete(&mut self) -> bool {
        if let Some(boundary) = &self.observed_boundary {
            self.final_response = boundary.result.response.clone();
            self.turn_error = boundary.result.error.clone();
            self.turn_complete = true;
            return true;
        }
        false
    }

    fn apply_completion_effect(&mut self, effect: CompletionDurableEffect) -> bool {
        self.observed_boundary = effect.observed_boundary;
        if effect.turn_complete {
            self.final_response = effect.final_response;
            self.turn_error = effect.turn_error;
            self.turn_complete = true;
            return true;
        }
        if let Some(err) = effect.completion_error {
            self.completion_error = Some(err);
            return true;
        }
        false
    }

    fn into_outcome(self, state: FollowStreamState) -> FollowLoopOutcome {
        FollowLoopOutcome {
            final_response: self.final_response,
            turn_error: self.turn_error,
            turn_complete: self.turn_complete,
            observed_boundary: self.observed_boundary,
            was_cancelled: self.was_cancelled,
            cancel_confirmed: self.cancel_confirmed,
            saw_terminal_model_error: self.saw_terminal_model_error,
            completion_error: self.completion_error,
            cached_effective: state.cached_effective,
            pending_advisories: state.pending_advisories,
            emitted_logical_seqs: state.emitted_logical_seqs,
        }
    }
}

impl NatsSession {
    pub(super) async fn run_follow_stream_loop(
        &self,
        params: FollowLoopParams<'_>,
        event_stream: &mut SessionEventStream,
    ) -> Result<FollowLoopOutcome> {
        let mut session = self.init_loop_session(params);
        while !session.rt.check_boundary_complete() {
            if self.poll_follow_stream(&mut session, event_stream).await? {
                break;
            }
        }
        Ok(session.rt.into_outcome(session.state))
    }

    async fn poll_follow_stream(
        &self,
        session: &mut FollowLoopSession<'_>,
        event_stream: &mut SessionEventStream,
    ) -> Result<bool> {
        tokio::select! {
            _ = wait_abort_signal(&self.abort_signal) => {
                self.handle_cancel_signal(session.cx.target.prompt_seq, session.cx.live, &mut session.rt).await?;
                Ok(true)
            }
            Some(()) = wait_pending_cancel(&mut session.pending_cancel) => {
                self.handle_cancel_signal(session.cx.target.prompt_seq, session.cx.live, &mut session.rt).await?;
                Ok(true)
            }
            Some(update) = session.updates.next() => {
                let mut driver = LoopUpdateDriver {
                    driver: FollowStreamDriver { stream: event_stream, state: &mut session.state },
                    rt: &mut session.rt,
                };
                self.handle_loop_completion(update, &session.cx, &mut driver).await
            }
            maybe_envelope = event_stream.next() => {
                let mut driver = LoopUpdateDriver {
                    driver: FollowStreamDriver { stream: event_stream, state: &mut session.state },
                    rt: &mut session.rt,
                };
                Ok(Self::handle_loop_advisory(maybe_envelope, &session.cx, &mut driver))
            }
        }
    }

    fn init_loop_session<'a>(&self, params: FollowLoopParams<'a>) -> FollowLoopSession<'a> {
        let cached_effective = if params.initial_boundary.is_some() {
            harnx_core::session_reconstruct::apply_log_mutations_nats(&params.initial_entries).ok()
        } else {
            None
        };
        FollowLoopSession {
            state: FollowStreamState {
                cached_effective,
                pending_advisories: VecDeque::new(),
                emitted_logical_seqs: HashSet::new(),
                emitted_subagent_completions: HashSet::new(),
            },
            rt: FollowLoopRuntime::new(params.initial_boundary),
            updates: Box::pin(crate::nats_session::completion::updates(
                self.jetstream.clone(),
                self.storage_key.clone(),
                params.initial_entries,
                params.options.orphan_timeout,
                self.lease_acquisition_timeout,
            )),
            pending_cancel: params.pending_cancel,
            cx: FollowLoopContext {
                target: params.target,
                boundary_options: params.boundary_options,
                event_sink: params.event_sink,
                live: params.live,
            },
        }
    }

    async fn handle_cancel_signal(
        &self,
        prompt_seq: u64,
        live: &LiveEventState,
        rt: &mut FollowLoopRuntime,
    ) -> Result<()> {
        rt.was_cancelled = true;
        log::info!("nats session: cancel received, publishing cancel");
        if let Some(cancel_seq) = self
            .interrupt_prompt(prompt_seq, "client cancel")
            .await?
            .cancel_seq()
        {
            live.accept_interrupt(cancel_seq);
            rt.cancel_confirmed = true;
        }
        Ok(())
    }

    async fn handle_loop_completion(
        &self,
        update: Result<crate::nats_session::completion::DurableUpdate>,
        cx: &FollowLoopContext<'_>,
        driver: &mut LoopUpdateDriver<'_, '_>,
    ) -> Result<bool> {
        match update {
            Ok(update) => {
                let effect = self
                    .handle_completion_durable_update(update, cx, &mut driver.driver)
                    .await?;
                Ok(driver.rt.apply_completion_effect(effect))
            }
            Err(error) => {
                driver.rt.completion_error = Some(error);
                Ok(true)
            }
        }
    }

    fn handle_loop_advisory(
        maybe_envelope: Option<AdvisoryEnvelope>,
        cx: &FollowLoopContext<'_>,
        driver: &mut LoopUpdateDriver<'_, '_>,
    ) -> bool {
        match maybe_envelope {
            Some(envelope) => {
                if driver.driver.stream.should_render(&envelope) {
                    driver.rt.saw_terminal_model_error |=
                        matches!(envelope.event, AgentEvent::Model(ModelEvent::Error(_)));
                    driver
                        .driver
                        .state
                        .pending_advisories
                        .push_back(envelope.clone());
                    crate::nats_session::flush_pending_advisories(
                        &mut driver.driver.state.pending_advisories,
                        driver.driver.state.cached_effective.as_deref(),
                        cx.event_sink,
                        &mut driver.driver.state.emitted_logical_seqs,
                        crate::nats_session::AdvisoryFlush::Live,
                        cx.live,
                        driver.driver.stream.last_applied_seq(),
                    );
                }
                false
            }
            None => {
                log::info!("nats session: event stream closed");
                true
            }
        }
    }
}

async fn wait_pending_cancel(pending: &mut Option<mpsc::Receiver<()>>) -> Option<()> {
    match pending {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}
