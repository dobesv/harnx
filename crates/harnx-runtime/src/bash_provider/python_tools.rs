//! `tools` for the `bash` tool's Python: each tool is called through the
//! run's `InnerCallRun`, so the allowlist, the call budget, the trace and
//! media handling are the ones Bash tool commands use.

use std::sync::{Arc, OnceLock};

use harnx_core::{abort::AbortSignal, tool::ToolDeclaration};
use harnx_script::{
    commands::{error_text, is_mcp_error},
    python_value, ExcType, ExtFunctionResult, MontyException, MontyObject, ScriptHostContext,
    ScriptPythonFunctions, ScriptPythonHandler, ScriptRun, ScriptStatus,
};
use serde_json::Value;

use crate::bash_inner::{InnerCallRun, InnerOutcome};
use crate::script_tool_provider::python::partial_excerpt;
use crate::script_tools::conversion::to_python;
use crate::script_tools::python_tools::{with_tools, PythonToolRunner};

/// What a finished call means for the script.
pub(super) enum Mapped {
    /// The call's value.
    Value(Value),
    /// A `RuntimeError` the script can catch.
    Raise(String),
    /// The run stops; the script cannot catch it.
    Stop(String),
}

/// `tools` for the script's Python, calling through `run`. A fatal call
/// records its message in `fatal` and stops the run.
pub(super) fn python_tools(
    run: Arc<InnerCallRun>,
    abort: AbortSignal,
    tools: &[ToolDeclaration],
    fatal: Arc<OnceLock<String>>,
) -> ScriptPythonFunctions {
    let base = ScriptPythonFunctions {
        names: Vec::new(),
        handler: Arc::new(NoHostFunctions),
        prelude: None,
    };
    with_tools(base, tools, Arc::new(Runner { run, abort, fatal }))
}

/// A stopped run whose stop came from a fatal call reports like a Bash tool
/// command's fatal: an error status carrying the message.
pub(super) fn report_fatal(result: &mut ScriptRun, fatal: Option<&String>) {
    let Some(message) = fatal else { return };
    if result.status != ScriptStatus::Stopped {
        return;
    }
    result.status = ScriptStatus::Error;
    result.exit_code = 1;
    result.stderr.push_str(&format!("{message}\n"));
    result.error = Some(message.clone());
}

/// Decides what an outcome means; makes no calls.
pub(super) fn map_outcome(tool: &str, outcome: InnerOutcome) -> Mapped {
    match outcome {
        InnerOutcome::Ok { result } if is_mcp_error(&result.output) => {
            Mapped::Raise(format!("{tool}: {}", error_text(&result.output)))
        }
        InnerOutcome::Ok { result } => Mapped::Value(python_value(&result.output)),
        InnerOutcome::ToolError {
            message,
            partial_result,
        } => Mapped::Raise(tool_error_message(tool, &message, partial_result.as_ref())),
        InnerOutcome::ApprovalRequired { .. } => Mapped::Raise(format!(
            "{tool}: needs approval; call {tool} directly, outside the script"
        )),
        InnerOutcome::Blocked { reason } => Mapped::Raise(format!("{tool}: blocked: {reason}")),
        InnerOutcome::Rejected { reason } => Mapped::Raise(format!("{tool}: {reason}")),
        InnerOutcome::Fatal { message } => Mapped::Stop(message),
    }
}

fn tool_error_message(tool: &str, message: &str, partial: Option<&Value>) -> String {
    match partial {
        Some(partial) => format!(
            "{tool}: {message}; partial result: {}",
            partial_excerpt(partial)
        ),
        None => format!("{tool}: {message}"),
    }
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
        let outcome = self.run.call(&tool, arguments, &self.abort).await;
        match map_outcome(&tool, outcome) {
            Mapped::Value(value) => match to_python(value) {
                Ok(object) => ExtFunctionResult::Return(object),
                Err(error) => raise(format!("{tool}: {error:#}")),
            },
            Mapped::Raise(message) => raise(message),
            Mapped::Stop(message) => {
                // A host stop cannot be caught by `try`/`except`, so the
                // script cannot swallow it and call another tool.
                let _ = self.fatal.set(message.clone());
                ctx.control.stop();
                raise(message)
            }
        }
    }
}

fn raise(message: String) -> ExtFunctionResult {
    ExtFunctionResult::Error(MontyException::new(ExcType::RuntimeError, Some(message)))
}

/// The `bash` tool's Python has no host functions besides `tools`.
struct NoHostFunctions;

#[async_trait::async_trait]
impl ScriptPythonHandler for NoHostFunctions {
    async fn call(
        &self,
        name: String,
        _args: Vec<MontyObject>,
        _kwargs: Vec<(MontyObject, MontyObject)>,
        _ctx: ScriptHostContext,
    ) -> ExtFunctionResult {
        ExtFunctionResult::Error(MontyException::new(
            ExcType::NameError,
            Some(format!("name '{name}' is not defined")),
        ))
    }
}
