use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use harnx_core::abort::AbortSignal;
use harnx_core::tool::{ToolError, ToolProgress, ToolProvider, ToolProviderOutput, ToolReplay};
use serde_json::{json, Value};

use super::provider::{ScriptToolInvocation, ScriptToolProvider};

#[async_trait]
impl ToolProvider for ScriptToolProvider {
    fn name(&self) -> &str {
        "script-tools"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.tools.contains_key(tool_name)
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.call_tool_with_progress(
            tool_name,
            arguments,
            harnx_core::tool::ToolCallOrigin::default(),
            abort,
            Arc::new(NoopProgress),
        )
        .await
    }

    async fn call_tool_with_progress(
        &self,
        tool_name: &str,
        arguments: Value,
        origin: harnx_core::tool::ToolCallOrigin<'_>,
        abort: &AbortSignal,
        progress: Arc<dyn ToolProgress>,
    ) -> Result<ToolProviderOutput, ToolError> {
        let tool = self.tools.get(tool_name).ok_or_else(|| {
            ToolError::Recoverable(anyhow!("provider does not handle tool '{tool_name}'"))
        })?;
        self.run_script_tool(
            tool,
            arguments,
            ScriptToolInvocation {
                tool_round: origin.tool_round,
                abort,
                progress,
            },
        )
        .await
    }

    /// Like `bash`, a script tool invocation is never re-run on worker loss:
    /// its side effects may have happened and only the root call's payload is
    /// durable. Answering `interrupted` here wins over NATS replay by
    /// provider order, and only for names this round's accepted view owns.
    async fn replay_tool_call(
        &self,
        replay: ToolReplay<'_>,
        _abort: &AbortSignal,
    ) -> Result<Option<ToolProviderOutput>, ToolError> {
        if !self.tools.contains_key(replay.call.name.as_str()) {
            return Ok(None);
        }
        let message = "script tool was interrupted: the worker restarted before results \
             were persisted; the script was not resumed and its side effects may have happened";
        Ok(Some(ToolProviderOutput::new(json!({
            "status": "interrupted",
            "error": message,
            "content": [{"type": "text", "text": message}],
        }))))
    }
}

/// Default progress sink for the plain `call_tool` entry point, matching the
/// other providers' behavior when no UI is attached.
struct NoopProgress;

impl ToolProgress for NoopProgress {
    fn update(&self, _patch: harnx_core::tool::ToolUpdatePatch) {}
}
