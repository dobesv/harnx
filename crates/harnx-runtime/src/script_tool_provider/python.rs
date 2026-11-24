//! Monty Python host functions for script-tool bodies: `tool_input()` and
//! `call_tool(name, args)`.
//!
//! Both route through the same machinery as the Bash body: argument
//! binding/conversion come from the shared `script_tools` helpers, and
//! `call_tool` goes through the same `InnerCallRun` the CLI commands use, so
//! the admission pipeline, budgets (8 concurrent, 1000 calls), payload
//! pre-flight and status classification hold by construction (decision 11).
//! Every non-success outcome raises a catchable Python exception whose
//! message names the tool and carries the error text; an uncaught exception
//! then fails the script run like any other non-zero exit. The exception is a
//! terminal `InnerOutcome::Fatal`: like the Bash command path, it stops the
//! whole run instead of raising, so `try`/`except` cannot swallow it and keep
//! calling tools.

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use harnx_core::abort::AbortSignal;
use harnx_script::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, ScriptHostContext,
    ScriptPythonFunctions, ScriptPythonHandler,
};
use serde_json::Value;

use crate::bash_inner::{InnerCallRun, InnerOutcome};
use crate::script_tools::conversion::to_python;
use crate::script_tools::python::{bind, tool_arguments};

const FUNCTIONS: &[&str] = &["tool_input", "call_tool"];

/// Longest partial-result excerpt appended to a tool-error exception, so an
/// inner failure's diagnostics stay as bounded as the Bash CLI's output.
const PARTIAL_EXCERPT_CHARS: usize = 256;

/// Python capabilities for one script-tool body run: `tool_input()` returns
/// the validated arguments and `call_tool(name, args)` dispatches through
/// `run`, the same `InnerCallRun` the Bash CLI commands use. `fatal` is the
/// caller-owned state the host contract requires: a terminal
/// `InnerOutcome::Fatal` records its message there and stops the host, and
/// the provider reads it back to report why the run stopped. A `OnceLock`
/// holds the first fatal only — the host is already stopping.
pub(super) fn functions(
    run: Arc<InnerCallRun>,
    abort: AbortSignal,
    arguments: Value,
    fatal: Arc<OnceLock<String>>,
) -> ScriptPythonFunctions {
    ScriptPythonFunctions {
        names: FUNCTIONS.iter().map(|name| (*name).to_string()).collect(),
        handler: Arc::new(Handler {
            run,
            abort,
            arguments,
            fatal,
        }),
    }
}

struct Handler {
    run: Arc<InnerCallRun>,
    abort: AbortSignal,
    arguments: Value,
    fatal: Arc<OnceLock<String>>,
}

#[async_trait::async_trait]
impl ScriptPythonHandler for Handler {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        match self.execute(&name, args, kwargs, &ctx).await {
            Ok(value) => ExtFunctionResult::Return(value),
            Err(error) => ExtFunctionResult::Error(MontyException::new(
                ExcType::RuntimeError,
                Some(format!("{error:#}")),
            )),
        }
    }
}

impl Handler {
    async fn execute(
        &self,
        name: &str,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        ctx: &ScriptHostContext,
    ) -> Result<MontyObject> {
        if name == "call_tool" {
            let (tool, arguments) = tool_arguments(args, kwargs)?;
            // Parent abort first, so a rejection-only loop still observes
            // cancellation the same way the Bash commands do.
            return match self.run.call(&tool, arguments, &self.abort).await {
                InnerOutcome::Ok { result } => to_python(result.output),
                InnerOutcome::ToolError {
                    message,
                    partial_result,
                } => Err(call_failed(&tool, tool_error_text(message, partial_result))),
                InnerOutcome::ApprovalRequired {
                    tool: approval_tool,
                    reason,
                } => Err(call_failed(
                    &tool,
                    format!(
                        "interaction required for '{approval_tool}': {}",
                        reason.unwrap_or_default()
                    ),
                )),
                InnerOutcome::Blocked { reason } => Err(call_failed(
                    &tool,
                    format!("denied by middleware: {reason}"),
                )),
                InnerOutcome::Rejected { reason } => {
                    Err(call_failed(&tool, format!("rejected: {reason}")))
                }
                InnerOutcome::Fatal { message } => {
                    // Terminal failure: record the message in caller-owned
                    // state, then stop the host. A host stop cannot be caught
                    // by Python `try`/`except`, so the script cannot swallow
                    // it and call another tool. The provider reads the
                    // recorded message out of `fatal` and reports it; the
                    // host is stopping, so a second fatal keeps the first.
                    let _ = self.fatal.set(message.clone());
                    ctx.control.stop();
                    Err(call_failed(&tool, message))
                }
            };
        }
        // tool_input takes no arguments; bind enforces that.
        bind(&[], args, kwargs)?;
        to_python(self.arguments.clone())
    }
}

/// Decision 11: the exception message must name the tool and carry the error
/// text, so an uncaught failure is diagnosable from the tool result.
fn call_failed(tool: &str, message: String) -> anyhow::Error {
    anyhow::anyhow!("call_tool '{tool}' failed: {message}")
}

/// A tool error's exception text: the error message plus a bounded excerpt of
/// the provider's partial result, mirroring the Bash CLI which prints the
/// partial result on stdout alongside the failure.
fn tool_error_text(message: String, partial_result: Option<Value>) -> String {
    let mut text = format!("tool error: {message}");
    if let Some(partial) = partial_result {
        if let Ok(serialized) = serde_json::to_string(&partial) {
            let excerpt = &serialized[..serialized
                .char_indices()
                .nth(PARTIAL_EXCERPT_CHARS)
                .map_or(serialized.len(), |(index, _)| index)];
            text.push_str("; partial result: ");
            text.push_str(excerpt);
        }
    }
    text
}
