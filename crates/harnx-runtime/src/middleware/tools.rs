use super::{effects::DispatchGuard, frame::Frame};
use anyhow::{anyhow, ensure, Result};
use harnx_core::tool::{ToolCall, ToolError};
use harnx_engine::tool::{eval_inner_tool_call, InnerToolEvalOptions};
use harnx_script::{
    commands::{CallOutcome, InnerExecutor, ToolSpec},
    CancellationToken, ScriptCommand,
};
use serde_json::Value;
use std::sync::Arc;

pub(super) fn commands(frame: &Arc<Frame>) -> Result<Vec<Arc<dyn ScriptCommand>>> {
    let specs = frame
        .tools
        .iter()
        .map(|tool| ToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone(),
            schema: tool.parameters.as_value().clone(),
        })
        .collect();
    Ok(harnx_script::commands::tool_commands(
        specs,
        Arc::new(FrameTools(frame.clone())),
    )?)
}

pub(super) struct FrameTools(pub Arc<Frame>);

impl FrameTools {
    fn durable<T>(&self, result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            let message = format!("{error:#}");
            self.0
                .terminal
                .lock()
                .get_or_insert(Err(ToolError::Fatal(error)));
            anyhow!(message)
        })
    }

    pub async fn execute(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let _operation = self.0.begin()?;
        ensure!(
            self.0.tools.iter().any(|tool| tool.name == name),
            "middleware tool '{name}' is not allowlisted"
        );
        ensure!(
            arguments.is_object(),
            "tool arguments must be a JSON object"
        );
        let context = &self.0.prepared.context;
        let mut ctx = context
            .evaluation
            .fork_for_inner_calls(InnerToolEvalOptions {
                providers: context.evaluation.providers.clone(),
                tool_round: context.evaluation.tool_round,
                emit_tool_call_fn: Arc::new(|_, _| {}),
                emit_tool_result_fn: Arc::new(|_, _| {}),
                emit_tool_blocked_fn: Arc::new(|_, _| {}),
                emit_tool_update_fn: Arc::new(|_, _| {}),
            });
        ctx.middleware_fn = None;
        ctx.allowed_tool_names = self.0.tools.iter().map(|tool| tool.name.clone()).collect();
        let call = ToolCall::new(
            name.to_string(),
            arguments,
            Some(uuid::Uuid::new_v4().to_string()),
            None,
        );
        self.dispatch(ctx, call, cancel).await
    }

    async fn dispatch(
        &self,
        ctx: harnx_engine::tool::ToolEvalContext,
        call: ToolCall,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let context = &self.0.prepared.context;
        let dispatch = DispatchGuard::begin(self.0.ancestors.clone());
        let physical = match &self.0.attempt {
            Some(attempt) => Some(
                self.durable(
                    attempt
                        .dispatch(
                            crate::interaction_checkpoint::records::DispatchKind::CallTool,
                            self.0.identity.step_path.clone(),
                            call.clone(),
                        )
                        .await,
                )?,
            ),
            None => None,
        };
        let evaluation = async {
            match &physical {
                Some(id) => {
                    crate::nats_tool_provider::with_physical_invocation(
                        id.clone(),
                        call.id.clone().unwrap_or_default(),
                        eval_inner_tool_call(&ctx, call, &context.abort),
                    )
                    .await
                }
                None => eval_inner_tool_call(&ctx, call, &context.abort).await,
            }
        };
        let evaluated = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(anyhow!("middleware tool call cancelled")),
            result = evaluation => result,
        };
        let evaluated = self.durable(evaluated)?;
        if let (Some(attempt), Some(id)) = (&self.0.attempt, physical) {
            self.durable(
                attempt
                    .completed(
                        &id,
                        &harnx_engine::tool::MaterializedToolResult::new(
                            evaluated.result.clone(),
                            evaluated.status,
                        ),
                    )
                    .await,
            )?;
        }
        dispatch.completed();
        Ok(evaluated.result.output)
    }
}

#[async_trait::async_trait]
impl InnerExecutor for FrameTools {
    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> CallOutcome {
        match self.execute(name, args, cancel).await {
            Ok(output) => CallOutcome::Ok(output),
            Err(error) => {
                let message = format!("{error:#}");
                self.0
                    .state
                    .lock()
                    .failure
                    .get_or_insert_with(|| message.clone());
                CallOutcome::Fatal { message }
            }
        }
    }
}
