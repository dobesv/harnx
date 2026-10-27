//! Typed suspension boundaries share the existing follower's event and cancel path.
use super::{
    AppendedPrompt, NatsSession, NatsTurnResult, PendingHitlApproval, PromptProgress,
    RunTurnOptions,
};
use crate::nats_event_sink::{LiveEventState, SessionEventStream};
use anyhow::{ensure, Context, Result};
use harnx_core::{event::AgentEventSink, session::SessionLogEntry};
use std::sync::Arc;

mod completion;
mod settle;
mod stream;

use completion::*;
use settle::*;
use stream::*;

/// A durable response boundary. InputRequired suspends the same invocation;
/// it doesn't close admission or end the turn.
#[derive(Clone, Debug)]
pub enum FollowBoundary {
    InputRequired(PendingHitlApproval),
    Terminal(NatsTurnResult),
}

#[derive(Debug, Default)]
pub struct FollowBoundaryOptions {
    /// Last handled request, not a log replay or terminal-detection cursor.
    /// A still-undecided request at or before this sequence is returned again.
    pub after_request_seq: Option<u64>,
    pub run: RunTurnOptions,
    pub pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
}

pub(super) struct FollowResult {
    pub result: NatsTurnResult,
    pub pending: Option<PendingHitlApproval>,
}

impl From<NatsTurnResult> for FollowResult {
    fn from(result: NatsTurnResult) -> Self {
        Self {
            result,
            pending: None,
        }
    }
}

pub(super) struct FollowTarget<'a> {
    pub invocation_id: &'a str,
    pub prompt_id: &'a str,
    pub prompt_seq: u64,
}

pub(super) struct FollowPromptOptions<'a> {
    pub appended: AppendedPrompt,
    pub event_sink: Arc<dyn AgentEventSink>,
    pub pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
    pub tool_confirmation_subject: Option<&'a str>,
    pub options: RunTurnOptions,
    pub boundary_options: Option<FollowBoundaryOptions>,
}

struct InitialFollowConfig<'a> {
    pub target: &'a FollowTarget<'a>,
    pub boundary_options: Option<&'a FollowBoundaryOptions>,
    pub tool_confirmation_subject: Option<&'a str>,
    pub token_budget: Option<u64>,
}

impl NatsSession {
    /// Follow an existing prompt until its next durable approval or terminal.
    /// No confirmation subject is installed, so workers defer approval requests.
    /// Live text and other advisory events go to the same sink as the legacy follower.
    pub async fn follow_admitted_prompt_until_boundary(
        &self,
        appended: AppendedPrompt,
        event_sink: Arc<dyn AgentEventSink>,
        options: FollowBoundaryOptions,
    ) -> Result<FollowBoundary> {
        appended
            .execution_id()
            .context("boundary follow requires an admitted invocation identity")?;
        let run_options = options.run;
        let pending_cancel = options.pending_cancel;
        let boundary_opts = FollowBoundaryOptions {
            after_request_seq: options.after_request_seq,
            run: run_options,
            pending_cancel: None,
        };
        let observed = self
            .follow_prompt(FollowPromptOptions {
                appended,
                event_sink,
                pending_cancel,
                tool_confirmation_subject: None,
                options: run_options,
                boundary_options: Some(boundary_opts),
            })
            .await?;
        Ok(match observed.pending {
            Some(pending) => FollowBoundary::InputRequired(pending),
            None => FollowBoundary::Terminal(observed.result),
        })
    }

    pub(super) fn observe_follow_boundary(
        &self,
        entries: &[(u64, SessionLogEntry)],
        target: &FollowTarget<'_>,
        options: &FollowBoundaryOptions,
    ) -> Result<Option<FollowResult>> {
        let scope = super::hitl::InvocationProgressScope {
            invocation_id: target.invocation_id,
            prompt_id: target.prompt_id,
            prompt_sequence: target.prompt_seq,
            session_id: &self.session_id,
        };
        Ok(
            match Self::invocation_progress_from_entries(&scope, entries)? {
                PromptProgress::Terminal(result) => Some(result.into()),
                PromptProgress::InputRequired(pending) => {
                    if options
                        .after_request_seq
                        .is_some_and(|cursor| pending.request_seq <= cursor)
                    {
                        log::debug!("returning still-undecided HITL request {} at or before follow cursor {:?}", pending.request_seq, options.after_request_seq);
                    }
                    let (response, error) = Self::extract_turn_outcome(entries, target.prompt_seq);
                    Some(FollowResult {
                        result: NatsTurnResult {
                            response,
                            error,
                            session_id: self.session_id.clone(),
                            was_cancelled: false,
                            user_msg_seq: target.prompt_seq,
                            user_msg_id: target.prompt_id.into(),
                        },
                        pending: Some(pending),
                    })
                }
                PromptProgress::Running { .. } => None,
            },
        )
    }

    pub(super) async fn follow_prompt(
        &self,
        options: FollowPromptOptions<'_>,
    ) -> Result<FollowResult> {
        let user_msg_seq = options.appended.user_msg_seq();
        let mut stopped_result = NatsTurnResult {
            response: None,
            session_id: self.session_id.clone(),
            was_cancelled: true,
            error: None,
            user_msg_seq,
            user_msg_id: options.appended.user_msg_id().to_string(),
        };
        let live = options.appended.live.clone();
        let result = tokio::select! {
            biased;
            cancel_seq = self.wait_for_prompt_interrupt(user_msg_seq) => {
                let (cancel_seq, timeout) = cancel_seq?;
                stopped_result.error = timeout;
                if let Some(live) = &live { live.accept_interrupt(cancel_seq); }
                return Ok(stopped_result.into());
            }
            result = Box::pin(self.follow_prompt_inner(options)) => result,
        };
        if result
            .as_ref()
            .is_ok_and(|result| result.result.was_cancelled)
        {
            return result;
        }
        if let Some((cancel_seq, timeout)) = self.prompt_interrupt_receipt(user_msg_seq).await? {
            stopped_result.error = timeout;
            if let Some(live) = &live {
                live.accept_interrupt(cancel_seq);
            }
            return Ok(stopped_result.into());
        }
        result
    }

    async fn follow_prompt_inner(&self, options: FollowPromptOptions<'_>) -> Result<FollowResult> {
        let mut appended = options.appended;
        let (mut event_stream, live) = self.prepare_follow_stream(&mut appended).await?;
        let invocation_id = appended.execution_id().unwrap_or("");
        let prompt_id = appended.user_msg_id().to_string();
        let prompt_seq = appended.user_msg_seq();
        let target = FollowTarget {
            invocation_id,
            prompt_id: &prompt_id,
            prompt_seq,
        };
        let (initial_entries, initial_boundary) = self
            .evaluate_initial_follow_boundary(
                &event_stream,
                &InitialFollowConfig {
                    target: &target,
                    boundary_options: options.boundary_options.as_ref(),
                    tool_confirmation_subject: options.tool_confirmation_subject,
                    token_budget: options.options.token_budget,
                },
            )
            .await?;
        let outcome = self
            .run_follow_stream_loop(
                FollowLoopParams {
                    target: &target,
                    event_sink: &options.event_sink,
                    pending_cancel: options.pending_cancel,
                    options: &options.options,
                    boundary_options: options.boundary_options.as_ref(),
                    initial_entries,
                    initial_boundary,
                    live: &live,
                },
                &mut event_stream,
            )
            .await?;
        self.settle_follow_stream(
            FollowSettleContext {
                target: &target,
                boundary_options: options.boundary_options.as_ref(),
                event_sink: options.event_sink,
                live,
            },
            event_stream,
            outcome,
        )
        .await
    }

    async fn take_retained_stream(appended: &AppendedPrompt) -> Option<SessionEventStream> {
        if let Some(events) = appended.events.as_ref() {
            events.lock().await.take()
        } else {
            None
        }
    }

    async fn attach_or_reuse_stream(
        &self,
        retained: Option<SessionEventStream>,
    ) -> Result<SessionEventStream> {
        match retained {
            Some(events) => Ok(events),
            None => SessionEventStream::attach(
                self.jetstream.clone(),
                self.client.clone(),
                &self.storage_key,
            )
            .await
            .context("failed to attach to session event stream"),
        }
    }

    async fn sync_stream_live_state(
        stream: &mut SessionEventStream,
        live: Option<LiveEventState>,
    ) -> Result<LiveEventState> {
        if let Some(live) = live {
            stream.use_live_state(live).await?;
        } else {
            stream.refresh_generation().await?;
        }
        Ok(stream.live_state().clone())
    }

    async fn prepare_follow_stream(
        &self,
        appended: &mut AppendedPrompt,
    ) -> Result<(SessionEventStream, LiveEventState)> {
        let retained = Self::take_retained_stream(appended).await;
        let mut event_stream = self.attach_or_reuse_stream(retained).await?;
        let live = Self::sync_stream_live_state(&mut event_stream, appended.live.take()).await?;
        Ok((event_stream, live))
    }

    async fn load_initial_follow_entries(
        &self,
        event_stream: &SessionEventStream,
        has_boundary_options: bool,
    ) -> Result<Vec<(u64, SessionLogEntry)>> {
        if has_boundary_options {
            crate::nats_session::completion::bounded_refresh(self.load_durable_entries()).await
        } else {
            Ok(event_stream.history().to_vec())
        }
    }

    fn check_initial_follow_boundary(
        &self,
        entries: &[(u64, SessionLogEntry)],
        target: &FollowTarget<'_>,
        boundary_options: Option<&FollowBoundaryOptions>,
    ) -> Result<Option<FollowResult>> {
        let Some(boundary_options) = boundary_options else {
            return Ok(None);
        };
        ensure!(
            !target.invocation_id.is_empty(),
            "boundary follow has no invocation identity"
        );
        self.observe_follow_boundary(entries, target, boundary_options)
    }

    async fn evaluate_initial_follow_boundary(
        &self,
        event_stream: &SessionEventStream,
        config: &InitialFollowConfig<'_>,
    ) -> Result<(Vec<(u64, SessionLogEntry)>, Option<FollowResult>)> {
        let initial_entries = self
            .load_initial_follow_entries(event_stream, config.boundary_options.is_some())
            .await?;
        let initial_boundary = self.check_initial_follow_boundary(
            &initial_entries,
            config.target,
            config.boundary_options,
        )?;
        if initial_boundary.is_none() {
            self.publish_activation(
                config.target.prompt_seq,
                config.tool_confirmation_subject,
                config.token_budget,
            )
            .await?;
        }
        Ok((initial_entries, initial_boundary))
    }
}
