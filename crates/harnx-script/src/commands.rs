//! Tool commands and discovery, independent of runtime admission and transport.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    cli_map::{parse_args, split_output_args, UsageError},
    help, CancellationToken, ScriptCommand, ScriptCommandContext, ScriptCommandOutput,
};

mod discovery;
mod render;
pub(crate) use discovery::unknown_command_message;

/// The declaration subset needed for parsing and discovery. Names stay identical
/// to direct tool names; only property names become kebab-case flags.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// Admission stays with the embedder. All commands in a run share this executor,
/// so its concurrency and call-budget limits apply across pipes/background jobs.
#[async_trait]
pub trait InnerExecutor: Send + Sync {
    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> CallOutcome;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionKind {
    UnknownCommand,
    Other,
}

/// Success contains the raw provider payload (MCP CallToolResult or plain JSON),
/// not a serialized runtime ToolResult envelope. Fatal always stops the run.
#[derive(Clone, Debug)]
pub enum CallOutcome {
    Ok(Value),
    ToolError {
        message: String,
        partial_result: Option<Value>,
    },
    ApprovalRequired,
    Blocked {
        reason: String,
    },
    Rejected {
        reason: String,
        kind: RejectionKind,
    },
    Fatal {
        message: String,
    },
}

type Catalog = BTreeMap<String, ToolSpec>;

/// Register a fixed catalog plus `tools` and `help`. Duplicate names and names
/// reserved for discovery are errors, rather than silently replacing commands.
/// Tool names may override BashKit builtins, matching direct tool names exactly.
pub fn tool_commands(
    tools: Vec<ToolSpec>,
    executor: Arc<dyn InnerExecutor>,
) -> Result<Vec<Arc<dyn ScriptCommand>>, UsageError> {
    let mut catalog = Catalog::new();
    for tool in tools {
        if tool.name.is_empty()
            || tool
                .name
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '/')
            || matches!(tool.name.as_str(), "tools" | "help")
        {
            return Err(UsageError::new(format!(
                "invalid or reserved tool command name {:?}",
                tool.name
            )));
        }
        let name = tool.name.clone();
        if catalog.insert(name.clone(), tool).is_some() {
            return Err(UsageError::new(format!("duplicate tool command {name:?}")));
        }
    }
    let catalog = Arc::new(catalog);
    let mut commands: Vec<Arc<dyn ScriptCommand>> = catalog
        .values()
        .map(|tool| {
            Arc::new(ToolCommand {
                tool: tool.clone(),
                executor: executor.clone(),
            }) as Arc<dyn ScriptCommand>
        })
        .collect();
    commands.extend(discovery::commands(catalog));
    Ok(commands)
}

struct ToolCommand {
    tool: ToolSpec,
    executor: Arc<dyn InnerExecutor>,
}

#[async_trait]
impl ScriptCommand for ToolCommand {
    fn name(&self) -> &str {
        &self.tool.name
    }

    async fn run(
        &self,
        args: &[String],
        stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> Result<ScriptCommandOutput, String> {
        // Split the output-format flags first, then check for the standalone
        // `--help`, so `tool --help -o text` and `tool -o json --help` also
        // request help instead of a usage error (PR3 review N1). A tool
        // property named `help` remains usable with --help=VALUE or --json.
        let (args, format) = match split_output_args(&self.tool.schema, args) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(failure(2, error.message)),
        };
        if args == ["--help"] {
            return Ok(ScriptCommandOutput {
                stdout: help::help(&self.tool.name, &self.tool.schema, &self.tool.description),
                ..Default::default()
            });
        }
        // Invalid UTF-8 stdin must remain invisible without --json -. Lossy
        // conversion would silently alter JSON string values when it is requested.
        let stdin = stdin.and_then(|bytes| std::str::from_utf8(bytes).ok());
        let args = match parse_args(&self.tool.schema, &args, stdin) {
            Ok(args) => args,
            Err(error) => return Ok(failure(2, error.message)),
        };
        let outcome = self.executor.call(&self.tool.name, args, ctx.cancel).await;
        let output = match outcome {
            CallOutcome::Ok(value) => {
                // MCP domain errors arrive over a successful transport. Don't
                // mistake arbitrary raw JSON `is_error` keys for engine status.
                if render::is_mcp_error(&value) {
                    failure(1, render::error_text(&value))
                } else {
                    ScriptCommandOutput {
                        stdout: render::output(&value, format),
                        ..Default::default()
                    }
                }
            }
            CallOutcome::ToolError {
                message,
                partial_result,
            } => {
                let mut output = failure(1, message);
                if let Some(partial) = partial_result {
                    output.stdout = render::output(&partial, format);
                }
                output
            }
            CallOutcome::ApprovalRequired => failure(
                3,
                format!(
                    "needs approval: {}; call it directly instead of from bash",
                    self.tool.name
                ),
            ),
            CallOutcome::Blocked { reason } => failure(4, reason),
            CallOutcome::Rejected { reason, kind } => failure(
                if kind == RejectionKind::UnknownCommand {
                    127
                } else {
                    5
                },
                reason,
            ),
            CallOutcome::Fatal { message } => return Err(message),
        };
        Ok(output)
    }
}

fn failure(exit_code: i32, message: impl AsRef<str>) -> ScriptCommandOutput {
    ScriptCommandOutput {
        exit_code,
        stderr: format!("{}\n", message.as_ref().trim_end()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
