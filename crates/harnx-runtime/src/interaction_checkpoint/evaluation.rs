use super::{
    attempt::{self, Attempt},
    live::DurableRound,
    records::{AttemptStart, DispatchKind},
};
use anyhow::{Context, Result};
use harnx_core::tool::{ToolError, ToolResult};
use harnx_engine::tool::{
    InnerToolCallStatus, MaterializedToolResult, PreparedToolCall, ToolCallNext, ToolMiddlewareFn,
    ToolMiddlewareOutcome,
};
use std::sync::Arc;

impl DurableRound {
    pub(super) fn middleware(&self, inner: Option<Arc<ToolMiddlewareFn>>) -> Arc<ToolMiddlewareFn> {
        let round = self.clone();
        Arc::new(move |prepared, origin, leaf| {
            let round = round.clone();
            let inner = inner.clone();
            Box::pin(async move {
                if origin != harnx_engine::tool::ToolMiddlewareOrigin::Model {
                    return match inner {
                        Some(inner) => inner(prepared, origin, leaf).await,
                        None => leaf(prepared.arguments.clone()).await,
                    };
                }
                round.ensure_started().await.map_err(ToolError::Fatal)?;
                if round
                    .chain_changed_after_dispatch(&prepared.call)
                    .await
                    .map_err(ToolError::Fatal)?
                {
                    return Ok(middleware_changed(prepared.call.clone()));
                }
                let resume = round
                    .resume_call(&prepared.call)
                    .await
                    .map_err(ToolError::Fatal)?;
                if let Some(saved) = &resume {
                    if let Some(interaction) = &saved.waiting {
                        return Ok(ToolMiddlewareOutcome::InputRequired {
                            interaction: interaction.clone(),
                            checkpoint: saved.resume.checkpoint.clone(),
                        });
                    }
                }
                let attempt = round
                    .start_attempt(&prepared, resume.as_ref().map(|s| s.reference.clone()))
                    .await
                    .map_err(ToolError::Fatal)?;
                let tool_input = resume
                    .as_ref()
                    .and_then(|saved| saved.resume.tool_input.clone());
                let leaf = round.leaf(&prepared, leaf, attempt.clone(), tool_input);
                let arguments = prepared.arguments.clone();
                let evaluation = async move {
                    match inner {
                        Some(inner) => inner(prepared, origin, leaf).await,
                        None => leaf(arguments).await,
                    }
                };
                let evaluation = attempt::scope(attempt, evaluation);
                match resume {
                    Some(saved) => crate::middleware::with_resume(saved.resume, evaluation).await,
                    None => evaluation.await,
                }
            })
        })
    }

    async fn start_attempt(
        &self,
        prepared: &PreparedToolCall,
        checkpoint: Option<String>,
    ) -> Result<Attempt> {
        Attempt::start(
            self.store.clone(),
            self.session.lease.clone(),
            AttemptStart {
                version: 1,
                session_storage_key: self.session.backend.session_id().into(),
                tool_round_seq: self.seq,
                tool_call_id: prepared
                    .call
                    .id
                    .clone()
                    .context("missing attempt call ID")?,
                attempt_id: uuid::Uuid::new_v4().to_string(),
                chain_version: self.chain_version.clone(),
                checkpoint,
                fence_token: self.session.lease.fence_token(),
            },
        )
        .await
    }

    fn leaf(
        &self,
        prepared: &PreparedToolCall,
        leaf: Arc<ToolCallNext>,
        attempt: Attempt,
        tool_input: Option<crate::middleware::ToolInputResume>,
    ) -> Arc<ToolCallNext> {
        let call = prepared.call.clone();
        let round = self.clone();
        Arc::new(move |arguments| {
            let mut call = call.clone();
            call.arguments = arguments.clone();
            let leaf = leaf.clone();
            let attempt = attempt.clone();
            let round = round.clone();
            let tool_input = tool_input.clone();
            Box::pin(async move {
                let tool_call_id = call.id.clone().unwrap_or_default();
                let id = attempt
                    .dispatch(DispatchKind::Next, attempt::step_path(), call.clone())
                    .await
                    .map_err(ToolError::Fatal)?;
                let dispatched = crate::nats_tool_provider::with_physical_invocation(
                    id.clone(),
                    tool_call_id.clone(),
                    leaf(arguments),
                );
                let outcome = match tool_input {
                    // Rerun after a tool challenge: only the owning call's
                    // leaf dispatch sees the accepted responses and state,
                    // never an allowlisted `call_tool` dispatch.
                    Some(input) => crate::middleware::with_tool_input(input, dispatched).await?,
                    None => dispatched.await?,
                };
                Self::settle(&round, &attempt, &id, call, outcome).await
            })
        })
    }

    /// Convert a leaf outcome into the durable layer's outcome: a raw tool
    /// challenge gains its D6 identity and checkpoint here, a completed call
    /// is externalized and journaled, and middleware-produced suspensions
    /// pass through with the checkpoint their chain runner minted.
    async fn settle(
        round: &DurableRound,
        attempt: &Attempt,
        id: &str,
        call: harnx_core::tool::ToolCall,
        outcome: ToolMiddlewareOutcome,
    ) -> Result<ToolMiddlewareOutcome, ToolError> {
        // The durable tool-call identity the challenge and its cap count on:
        // the same leaf dispatch id `with_physical_invocation` used.
        let tool_call_id = call.id.clone().unwrap_or_default();
        let outcome = match outcome {
            ToolMiddlewareOutcome::ToolInputRequired(input) => {
                // MRTR round cap (SEP-2322): a tool that keeps re-challenging
                // would keep the call suspended forever. After
                // MAX_TOOL_INPUT_ROUNDS challenges for one call the runtime
                // gives up with an error result instead, matching rmcp's
                // DEFAULT_MRTR_MAX_ROUNDS client default.
                if round
                    .tool_input_rounds(&tool_call_id)
                    .await
                    .map_err(ToolError::Fatal)?
                    >= harnx_toolset::MAX_TOOL_INPUT_ROUNDS
                {
                    return Ok(ToolMiddlewareOutcome::Completed(input_rounds_exceeded(
                        call,
                    )));
                }
                // The producer and key are tool-supplied infrastructure
                // facts; the session/round/call/step path belongs to the
                // durable layer, and the request state goes to the call's
                // checkpoint for the rerun.
                let context = crate::middleware::ToolChallengeContext {
                    session: round.session.backend.session_id().to_string(),
                    tool_round_seq: round.seq,
                    tool_call_id: tool_call_id.to_string(),
                    chain_version: round.chain_version.clone(),
                    step_path: attempt::step_path(),
                };
                crate::middleware::tool_challenge_outcome(&input, &context)
                    .map_err(ToolError::Fatal)?
            }
            ToolMiddlewareOutcome::Completed(mut result) => {
                round
                    .externalize(&mut result.result)
                    .await
                    .map_err(ToolError::Fatal)?;
                attempt
                    .completed(id, &result)
                    .await
                    .map_err(ToolError::Fatal)?;
                ToolMiddlewareOutcome::Completed(result)
            }
            // Middleware-produced suspensions already carry their
            // checkpoint from the chain runner's lift.
            // `ToolMiddlewareOutcome` is non-exhaustive across crates;
            // unknown future outcomes pass through unchanged.
            other => other,
        };
        Ok(outcome)
    }
}

fn middleware_changed(call: harnx_core::tool::ToolCall) -> ToolMiddlewareOutcome {
    ToolMiddlewareOutcome::Completed(MaterializedToolResult::new(
        ToolResult::new(
            call,
            serde_json::json!({"is_error": true,
            "error": "middleware changed while the call was waiting; downstream effects may already have happened"}),
        ),
        InnerToolCallStatus::ToolError,
    ))
}

/// The error result for a call whose tool kept challenging past the MRTR
/// round cap: visible failure, never a silent success or endless suspension.
fn input_rounds_exceeded(call: harnx_core::tool::ToolCall) -> MaterializedToolResult {
    MaterializedToolResult::new(
        ToolResult::new(
            call,
            serde_json::json!({"is_error": true,
            "error": format!("tool challenged for user input more than {} times without finishing; giving up",
                harnx_toolset::MAX_TOOL_INPUT_ROUNDS)}),
        ),
        InnerToolCallStatus::ToolError,
    )
}
