//! Reserved `bash` tool name for script mode (#2537).
//!
//! `bash` is script mode's tool name. While an agent's `bash_tools` resolves
//! to a non-empty inner set, the name is reserved: the model sees the
//! synthesized declaration (config::bash_declaration) and a `bash` call must
//! never fall through to an external tool of the same name — NATS registers
//! raw-name aliases and would win provider dispatch otherwise. This provider
//! claims the name ahead of the NATS provider and answers with a recoverable
//! error until the script backend exists; the real `BashProvider` replaces
//! this stub and inherits the same position and gating.

use crate::config::BASH_TOOL_NAME;
use anyhow::anyhow;
use async_trait::async_trait;
use harnx_core::abort::AbortSignal;
use harnx_core::tool::{ToolError, ToolProvider, ToolProviderOutput};
use serde_json::Value;

/// Holds the reserved `bash` name while script mode is active for the agent.
///
/// Inert (`has_tool` always false) when the agent has no resolved inner set,
/// so agents that genuinely use an external `bash` tool keep working.
pub struct ReservedBashProvider {
    active: bool,
}

impl ReservedBashProvider {
    pub fn new(active: bool) -> Self {
        Self { active }
    }
}

#[async_trait]
impl ToolProvider for ReservedBashProvider {
    fn name(&self) -> &str {
        "reserved-bash"
    }

    fn has_tool(&self, tool_name: &str) -> bool {
        self.active && tool_name == BASH_TOOL_NAME
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        _arguments: Value,
        _abort: &AbortSignal,
    ) -> Result<ToolProviderOutput, ToolError> {
        Err(ToolError::Recoverable(anyhow!(
            "script mode is not available in this build yet; the reserved '{tool_name}' tool cannot run scripts"
        )))
    }
}
