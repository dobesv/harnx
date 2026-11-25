//! A fake tool provider shared by the `bash` and script-tool provider tests:
//! it records whether each call asked for a structured result and answers
//! with fields an output mapping can read.

use std::sync::{Arc, Mutex};

use harnx_core::{
    abort::AbortSignal,
    tool::{ToolCallOrigin, ToolError, ToolProgress, ToolProvider, ToolProviderOutput},
};
use serde_json::{json, Value};

/// Records whether each call's origin asked for a structured result, and
/// answers with a result a mapping can read.
pub(crate) struct StructuredProvider {
    pub(crate) origins: Mutex<Vec<bool>>,
}

#[async_trait::async_trait]
impl ToolProvider for StructuredProvider {
    fn name(&self) -> &str {
        "structured"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        tool_name == "faketool"
    }

    async fn call_tool(
        &self,
        _name: &str,
        _args: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        Ok(ToolProviderOutput::new(json!({
            "content": [{"type": "text", "text": "ignored"}],
            "structuredContent": {"text": "one\ntwo\nthree\n", "notes": ["2 more lines"]},
        })))
    }

    async fn call_tool_with_progress(
        &self,
        name: &str,
        args: Value,
        origin: ToolCallOrigin<'_>,
        abort: &AbortSignal,
        _progress: Arc<dyn ToolProgress>,
    ) -> Result<ToolProviderOutput, ToolError> {
        self.origins.lock().unwrap().push(origin.structured_result);
        self.call_tool(name, args, abort).await
    }
}
