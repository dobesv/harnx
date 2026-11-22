use super::{effects::DispatchGuard, frame::Frame};
use crate::script_tools::{tool_commands, ScriptTools};
use anyhow::{anyhow, Result};
use harnx_core::tool::{ToolCall, ToolError};
use harnx_script::{
    commands::{CallOutcome, InnerExecutor},
    CancellationToken, ScriptCommand,
};
use serde_json::Value;
use std::sync::Arc;

pub(super) fn commands(frame: &Arc<Frame>) -> Result<Vec<Arc<dyn ScriptCommand>>> {
    tool_commands(&frame.tools, Arc::new(FrameTools(frame.clone())))
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
        let tools = ScriptTools::middleware(
            self.0.prepared.context.clone(),
            self.0.tools.iter().map(|tool| tool.name.clone()).collect(),
        );
        let (ctx, call) = tools.prepare(name, arguments)?;
        self.dispatch(&tools, ctx, call, cancel).await
    }

    async fn dispatch(
        &self,
        tools: &ScriptTools,
        ctx: harnx_engine::tool::ToolEvalContext,
        call: ToolCall,
        cancel: CancellationToken,
    ) -> Result<Value> {
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
                        tools.evaluate(&ctx, call),
                    )
                    .await
                }
                None => tools.evaluate(&ctx, call).await,
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
