//! Run Claude-protocol subprocess hooks through an ordinary tool service.
//!
//! No legacy hook registry, dispatcher, or core HookResult dependency. Middleware
//! owns policy: it applies mutations and maps `ask` to its durable `confirm`.
mod process;
mod protocol;
mod specs;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use harnx_toolset::{ToolInvokeError, ToolSpec, Toolset};
pub use protocol::{Decision, HookRunResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

const MAX_BYTES: usize = 256 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    command: Vec<String>,
    #[serde(default)]
    persistent: bool,
    event: String,
    payload: Value,
    #[serde(default)]
    package_dir: Option<PathBuf>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl RunRequest {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.command.first().is_some_and(|p| !p.trim().is_empty()),
            "command must contain a nonempty program"
        );
        ensure!(self.payload.is_object(), "payload must be a JSON object");
        ensure!(
            !self
                .payload
                .as_object()
                .is_some_and(|p| p.contains_key("id")),
            "payload.id is reserved for JSONL correlation"
        );
        ensure!(
            matches!(
                self.event.as_str(),
                "PreToolUse"
                    | "PostToolUse"
                    | "PostToolUseFailure"
                    | "SessionStart"
                    | "UserPromptSubmit"
                    | "Stop"
                    | "StopFailure"
                    | "InstructionsLoaded"
                    | "CwdChanged"
            ),
            "unsupported Claude hook event: {}",
            self.event
        );
        ensure!(
            self.payload["hook_event_name"].is_null()
                || self.payload["hook_event_name"] == self.event,
            "event conflicts with payload.hook_event_name"
        );
        ensure!(
            (1..=300).contains(&self.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
            "timeout_secs must be between 1 and 300"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_BYTES,
            "hook request exceeds {MAX_BYTES} bytes"
        );
        self.cwd()?;
        Ok(())
    }

    fn cwd(&self) -> Result<PathBuf> {
        match &self.payload["cwd"] {
            Value::Null => std::env::current_dir().context("resolve hook cwd"),
            Value::String(path) => {
                let path = PathBuf::from(path);
                ensure!(path.is_absolute(), "payload.cwd must be absolute");
                Ok(path)
            }
            _ => anyhow::bail!("payload.cwd must be a string"),
        }
    }

    fn wire_payload(&self) -> Value {
        let mut payload = self.payload.clone();
        payload["hook_event_name"] = json!(self.event);
        payload
    }
}

/// Owns resident hook processes. Drop the toolset to retire them at shutdown.
#[derive(Default)]
pub struct ClaudeHookToolset {
    pool: process::Pool,
}

#[async_trait]
impl Toolset for ClaudeHookToolset {
    fn name(&self) -> &str {
        "claude_hook"
    }
    fn default_mcp_http_port(&self) -> u16 {
        3008
    }
    fn tools(&self) -> Vec<ToolSpec> {
        vec![specs::run()]
    }

    async fn invoke(
        &self,
        tool: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolInvokeError> {
        if tool != "run" {
            return Err(ToolInvokeError::Recoverable(format!(
                "unknown Claude hook tool: {tool}"
            )));
        }
        let request: RunRequest = serde_json::from_value(args).map_err(recoverable)?;
        request.validate().map_err(recoverable)?;
        let timeout = Duration::from_secs(request.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(ToolInvokeError::Fatal("Claude hook call cancelled".into())),
            result = tokio::time::timeout(timeout, Box::pin(self.pool.run(&request))) => {
                let result = result.map_err(|_| ToolInvokeError::Recoverable("Claude hook timed out; hook outcome may be unknown".into()))?
                    .map_err(|error| recoverable(format!("{error:#}")))?;
                serde_json::to_value(result).map_err(recoverable)
            }
        }
    }
}

fn recoverable(error: impl std::fmt::Display) -> ToolInvokeError {
    ToolInvokeError::Recoverable(format!("Claude hook: {error}"))
}

#[cfg(test)]
mod tests;
