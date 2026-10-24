//! Typed suspension boundaries share the existing follower's event and cancel path.
use super::{
    AppendedPrompt, NatsSession, NatsTurnResult, PendingHitlApproval, PromptProgress,
    RunTurnOptions,
};
use anyhow::{Context, Result};
use harnx_core::{event::AgentEventSink, session::SessionLogEntry};
use std::sync::Arc;

/// A durable response boundary. InputRequired suspends the same invocation;
/// it doesn't close admission or end the turn.
#[derive(Clone, Debug)]
pub enum FollowBoundary {
    InputRequired(PendingHitlApproval),
    Terminal(NatsTurnResult),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FollowBoundaryOptions {
    /// Last handled request, not a log replay or terminal-detection cursor.
    /// A still-undecided request at or before this sequence is returned again.
    pub after_request_seq: Option<u64>,
    pub run: RunTurnOptions,
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

impl NatsSession {
    /// Follow an existing prompt until its next durable approval or terminal.
    /// No confirmation subject is installed, so workers defer approval requests.
    /// Live text and other advisory events go to the same sink as the legacy follower.
    pub async fn follow_admitted_prompt_until_boundary(
        &self,
        appended: AppendedPrompt,
        event_sink: Arc<dyn AgentEventSink>,
        pending_cancel: Option<tokio::sync::mpsc::Receiver<()>>,
        options: FollowBoundaryOptions,
    ) -> Result<FollowBoundary> {
        appended
            .execution_id()
            .context("boundary follow requires an admitted invocation identity")?;
        let observed = self
            .follow_prompt(
                appended,
                event_sink,
                pending_cancel,
                None,
                options.run,
                Some(options),
            )
            .await?;
        Ok(match observed.pending {
            Some(pending) => FollowBoundary::InputRequired(pending),
            None => FollowBoundary::Terminal(observed.result),
        })
    }

    pub(super) fn observe_follow_boundary(
        &self,
        entries: &[(u64, SessionLogEntry)],
        invocation_id: &str,
        prompt_id: &str,
        prompt_seq: u64,
        options: FollowBoundaryOptions,
    ) -> Result<Option<FollowResult>> {
        Ok(
            match Self::invocation_progress_from_entries(
                invocation_id,
                prompt_id,
                prompt_seq,
                &self.session_id,
                entries,
            )? {
                PromptProgress::Terminal(result) => Some(result.into()),
                PromptProgress::InputRequired(pending) => {
                    if options
                        .after_request_seq
                        .is_some_and(|cursor| pending.request_seq <= cursor)
                    {
                        log::debug!("returning still-undecided HITL request {} at or before follow cursor {:?}", pending.request_seq, options.after_request_seq);
                    }
                    let (response, error) = Self::extract_turn_outcome(entries, prompt_seq);
                    Some(FollowResult {
                        result: NatsTurnResult {
                            response,
                            error,
                            session_id: self.session_id.clone(),
                            was_cancelled: false,
                            user_msg_seq: prompt_seq,
                            user_msg_id: prompt_id.into(),
                        },
                        pending: Some(pending),
                    })
                }
                PromptProgress::Running { .. } => None,
            },
        )
    }
}
