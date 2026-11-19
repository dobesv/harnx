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
                let leaf = round.leaf(&prepared, leaf, attempt.clone());
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
    ) -> Arc<ToolCallNext> {
        let call = prepared.call.clone();
        let round = self.clone();
        Arc::new(move |arguments| {
            let mut call = call.clone();
            call.arguments = arguments.clone();
            let leaf = leaf.clone();
            let attempt = attempt.clone();
            let round = round.clone();
            Box::pin(async move {
                let tool_call_id = call.id.clone().unwrap_or_default();
                let id = attempt
                    .dispatch(DispatchKind::Next, attempt::step_path(), call)
                    .await
                    .map_err(ToolError::Fatal)?;
                let mut outcome = crate::nats_tool_provider::with_physical_invocation(
                    id.clone(),
                    tool_call_id,
                    leaf(arguments),
                )
                .await?;
                if let ToolMiddlewareOutcome::Completed(result) = &mut outcome {
                    round
                        .externalize(&mut result.result)
                        .await
                        .map_err(ToolError::Fatal)?;
                    attempt
                        .completed(&id, result)
                        .await
                        .map_err(ToolError::Fatal)?;
                }
                Ok(outcome)
            })
        })
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
