//! Monty Python host functions for script-tool bodies: `tool_input()` and the
//! `tools` object.
//!
//! Both route through the same machinery as the Bash body: argument
//! binding/conversion come from the shared `script_tools` helpers, and a
//! `tools.NAME(...)` call goes through the same `InnerCallRun` the CLI
//! commands use, so the admission pipeline, budgets (8 concurrent, 1000
//! calls), payload pre-flight and status classification hold by construction
//! (decision 11).
//! Every non-success outcome raises a catchable Python exception whose
//! message names the tool and carries the error text; an uncaught exception
//! then fails the script run like any other non-zero exit. The exception is a
//! terminal `InnerOutcome::Fatal`: like the Bash command path, it stops the
//! whole run instead of raising, so `try`/`except` cannot swallow it and keep
//! calling tools.

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use harnx_core::{abort::AbortSignal, tool::ToolDeclaration};
use harnx_script::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, ScriptHostContext,
    ScriptPythonFunctions, ScriptPythonHandler,
};
use serde_json::Value;

use crate::bash_inner::{InnerCallRun, InnerOutcome};
use crate::script_tools::conversion::to_python;
use crate::script_tools::python::bind;
use crate::script_tools::python_tools::{with_tools, PythonToolRunner};

const FUNCTIONS: &[&str] = &["tool_input"];

/// Longest partial-result excerpt appended to a tool-error exception, so an
/// inner failure's diagnostics stay as bounded as the Bash CLI's output.
const PARTIAL_EXCERPT_CHARS: usize = 256;

/// Python capabilities for one script-tool body run: `tool_input()` returns
/// the validated arguments and `tools.NAME(...)` dispatches through
/// `run`, the same `InnerCallRun` the Bash CLI commands use. `fatal` is the
/// caller-owned state the host contract requires: a terminal
/// `InnerOutcome::Fatal` records its message there and stops the host, and
/// the provider reads it back to report why the run stopped. A `OnceLock`
/// holds the first fatal only — the host is already stopping.
pub(super) fn functions(
    run: Arc<InnerCallRun>,
    abort: AbortSignal,
    arguments: Value,
    declarations: &[ToolDeclaration],
    fatal: Arc<OnceLock<String>>,
) -> ScriptPythonFunctions {
    let base = ScriptPythonFunctions {
        names: FUNCTIONS.iter().map(|name| (*name).to_string()).collect(),
        handler: Arc::new(Handler { arguments }),
        prelude: None,
    };
    with_tools(base, declarations, Arc::new(Runner { run, abort, fatal }))
}

struct Handler {
    arguments: Value,
}

#[async_trait::async_trait]
impl ScriptPythonHandler for Handler {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        _ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        if name != "tool_input" {
            return ExtFunctionResult::Error(MontyException::new(
                ExcType::NameError,
                Some(format!("name '{name}' is not defined")),
            ));
        }
        // tool_input takes no arguments; bind enforces that.
        match bind(&[], args, kwargs).and_then(|_| to_python(self.arguments.clone())) {
            Ok(value) => ExtFunctionResult::Return(value),
            Err(error) => runtime_error(&error),
        }
    }
}

fn runtime_error(error: &anyhow::Error) -> ExtFunctionResult {
    ExtFunctionResult::Error(MontyException::new(
        ExcType::RuntimeError,
        Some(format!("{error:#}")),
    ))
}

struct Runner {
    run: Arc<InnerCallRun>,
    abort: AbortSignal,
    fatal: Arc<OnceLock<String>>,
}

#[async_trait::async_trait]
impl PythonToolRunner for Runner {
    async fn run_tool(
        &self,
        tool: String,
        arguments: Value,
        ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        // Parent abort first, so a rejection-only loop still observes
        // cancellation the same way the Bash commands do.
        let outcome = self.run.call(&tool, arguments, &self.abort).await;
        match self.outcome_value(&tool, outcome, &ctx) {
            Ok(value) => ExtFunctionResult::Return(value),
            Err(error) => runtime_error(&error),
        }
    }
}

impl Runner {
    fn outcome_value(
        &self,
        tool: &str,
        outcome: InnerOutcome,
        ctx: &ScriptHostContext,
    ) -> Result<MontyObject> {
        match outcome {
            InnerOutcome::Ok { result } => to_python(result.output),
            InnerOutcome::ToolError {
                message,
                partial_result,
            } => Err(call_failed(tool, tool_error_text(message, partial_result))),
            InnerOutcome::ApprovalRequired {
                tool: approval_tool,
                reason,
            } => Err(call_failed(
                tool,
                format!(
                    "interaction required for '{approval_tool}': {}",
                    reason.unwrap_or_default()
                ),
            )),
            InnerOutcome::Blocked { reason } => {
                Err(call_failed(tool, format!("denied by middleware: {reason}")))
            }
            InnerOutcome::Rejected { reason } => {
                Err(call_failed(tool, format!("rejected: {reason}")))
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
                Err(call_failed(tool, message))
            }
        }
    }
}

/// Decision 11: the exception message must name the tool and carry the error
/// text, so an uncaught failure is diagnosable from the tool result.
fn call_failed(tool: &str, message: String) -> anyhow::Error {
    anyhow::anyhow!("{tool} failed: {message}")
}

/// A tool error's exception text: the error message plus a bounded excerpt of
/// the provider's partial result, mirroring the Bash CLI which prints the
/// partial result on stdout alongside the failure.
fn tool_error_text(message: String, partial_result: Option<Value>) -> String {
    let mut text = format!("tool error: {message}");
    if let Some(partial) = partial_result {
        text.push_str("; partial result: ");
        text.push_str(&partial_excerpt(&partial));
    }
    text
}

/// A bounded JSON excerpt of a tool's partial result, for an error message.
pub(crate) fn partial_excerpt(partial: &Value) -> String {
    let serialized = serde_json::to_string(partial).unwrap_or_default();
    serialized.chars().take(PARTIAL_EXCERPT_CHARS).collect()
}
