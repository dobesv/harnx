//! Lifecycle operations and Python/Bash adapters share one run's effects.

use super::{tools::HookTools, RunState};
use crate::{config::ResolvedEventHook, event_hook_operations::names, script_tools::ScriptTools};
use anyhow::{anyhow, ensure, Result};
use harnx_core::hooks::HookEventKind;
use harnx_script::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, ScriptCommand, ScriptCommandContext,
    ScriptCommandOutput, ScriptHostContext, ScriptPythonFunctions, ScriptPythonHandler,
};
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn register(
    hook: &ResolvedEventHook,
    event: HookEventKind,
    input_json: &Value,
    state: Arc<Mutex<RunState>>,
    tools: HookTools,
) -> (Vec<Arc<dyn ScriptCommand>>, ScriptPythonFunctions) {
    let commands: Vec<Arc<Operation>> = names(event)
        .iter()
        .map(|name| {
            Arc::new(Operation {
                name,
                state: state.clone(),
                input: input_json.clone(),
                fetch: hook.fetch.clone(),
            })
        })
        .collect();
    let mut script_commands = tools.commands;
    script_commands.extend(
        commands
            .iter()
            .map(|operation| Arc::new(BashOperation(operation.clone())) as Arc<dyn ScriptCommand>),
    );
    let python_functions = ScriptPythonFunctions {
        names: names(event)
            .iter()
            .copied()
            .chain(tools.executor.as_ref().map(|_| "call_tool"))
            .map(str::to_string)
            .collect(),
        handler: Arc::new(PythonOperation {
            operations: commands,
            tools: tools.executor,
        }),
    };
    (script_commands, python_functions)
}

/// One registered operation. The Bash command and Python function paths share
/// `execute`; only the value↔text conversion differs.
struct Operation {
    name: &'static str,
    state: Arc<Mutex<RunState>>,
    input: Value,
    fetch: Vec<String>,
}

struct BashOperation(Arc<Operation>);

#[async_trait::async_trait]
impl ScriptCommand for BashOperation {
    fn name(&self) -> &str {
        self.0.name
    }

    async fn run(
        &self,
        args: &[String],
        _stdin: Option<&[u8]>,
        ctx: ScriptCommandContext<'_>,
    ) -> std::result::Result<ScriptCommandOutput, String> {
        let host = ScriptHostContext {
            cancel: ctx.cancel,
            control: ctx.control,
        };
        match self.0.execute(args, &host).await {
            Ok(Some(value)) => Ok(ScriptCommandOutput {
                stdout: format!("{value}\n"),
                ..Default::default()
            }),
            Ok(None) => Ok(ScriptCommandOutput::default()),
            Err(error) => {
                host.control.stop();
                Err(format!("{error:#}"))
            }
        }
    }
}

impl Operation {
    async fn execute(&self, args: &[String], host: &ScriptHostContext) -> Result<Option<Value>> {
        match self.name {
            "event_input" => {
                ensure!(args.is_empty(), "event_input takes no arguments");
                Ok(Some(self.input.clone()))
            }
            "fetch" => {
                ensure!(args.len() == 1, "fetch requires one HTTP(S) URL");
                crate::middleware::fetch::fetch_url(&args[0], &self.fetch, &host.cancel)
                    .await
                    .map(Some)
            }
            "add_context" => {
                let message = args.join(" ");
                ensure!(!message.trim().is_empty(), "add_context requires a message");
                self.state.lock().context.push(message);
                Ok(None)
            }
            "block" => {
                let reason = args.join(" ");
                ensure!(!reason.trim().is_empty(), "block requires a reason");
                self.state.lock().block = Some(reason);
                Ok(None)
            }
            "rewrite_prompt" => {
                let prompt = args.join(" ");
                ensure!(
                    !prompt.trim().is_empty(),
                    "rewrite_prompt requires a prompt"
                );
                self.state.lock().rewrite = Some(prompt);
                Ok(None)
            }
            "resume" => {
                // The message is optional: an empty resume keeps the loop's
                // default continuation text.
                self.state.lock().resume = Some(args.join(" "));
                Ok(None)
            }
            _ => Err(anyhow!("unknown event-hook operation")),
        }
    }

    fn parameters(&self) -> &'static [&'static str] {
        match self.name {
            "event_input" => &[],
            "fetch" => &["url"],
            "add_context" => &["message"],
            "block" => &["reason"],
            "rewrite_prompt" => &["prompt"],
            "resume" => &["message"],
            _ => &[],
        }
    }
}

struct PythonOperation {
    operations: Vec<Arc<Operation>>,
    tools: Option<Arc<ScriptTools>>,
}

#[async_trait::async_trait]
impl ScriptPythonHandler for PythonOperation {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        host: ScriptHostContext,
    ) -> ExtFunctionResult {
        let result = async {
            if name == "call_tool" {
                let tools = self
                    .tools
                    .as_ref()
                    .ok_or_else(|| anyhow!("event-hook tools are not enabled"))?;
                let (name, arguments) = crate::script_tools::python::tool_arguments(args, kwargs)?;
                return tools
                    .execute(&name, arguments, host.cancel.clone())
                    .await
                    .map(Some);
            }
            let operation = self
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .ok_or_else(|| anyhow!("unknown event-hook function '{name}'"))?;
            let values = bind(operation.parameters(), args, kwargs)?;
            let args = values.iter().map(|value| match value {
                Value::String(text) => text.clone(),
                _ => String::new(),
            });
            operation.execute(&args.collect::<Vec<_>>(), &host).await
        }
        .await;
        match result.and_then(|value| {
            value
                .map(crate::script_tools::conversion::to_python)
                .transpose()
        }) {
            Ok(object) => ExtFunctionResult::Return(object.unwrap_or(MontyObject::None)),
            Err(error) => {
                host.control.stop();
                ExtFunctionResult::Error(MontyException::new(
                    ExcType::RuntimeError,
                    Some(format!("{error:#}")),
                ))
            }
        }
    }
}

/// Bind positional and keyword arguments to the operation's parameters,
/// mirroring the middleware Python bridge.
fn bind(
    parameters: &[&str],
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> Result<Vec<Value>> {
    ensure!(
        args.len() <= parameters.len(),
        "too many positional event-hook arguments"
    );
    let mut values = parameters
        .iter()
        .zip(args)
        .map(|(_, value)| crate::script_tools::conversion::from_python(value))
        .collect::<Result<Vec<_>>>()?;
    values.resize_with(parameters.len(), || Value::Null);
    for (key, value) in kwargs {
        let name = match crate::script_tools::conversion::from_python(key)? {
            Value::String(name) => name,
            _ => anyhow::bail!("event-hook keyword names must be strings"),
        };
        let index = parameters
            .iter()
            .position(|parameter| *parameter == name)
            .ok_or_else(|| anyhow!("unknown event-hook argument '{name}'"))?;
        values[index] = crate::script_tools::conversion::from_python(value)?;
    }
    Ok(values)
}
