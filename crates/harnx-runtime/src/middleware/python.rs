use super::{frame::Frame, tools::FrameTools};
use crate::script_tools::{
    conversion::to_python,
    python::{bind, required, string},
    python_tools::{with_tools, PythonToolRunner},
};
use anyhow::{bail, Result};
use harnx_script::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, ScriptHostContext,
    ScriptPythonFunctions, ScriptPythonHandler,
};
use serde_json::Value;
use std::sync::Arc;

const FUNCTIONS: &[&str] = &[
    "tool_input",
    "fetch",
    "call_next",
    "respond",
    "deny",
    "confirm",
    "answer",
    "save",
    "saved",
    "interact",
];

pub(super) fn functions(frame: &Arc<Frame>) -> ScriptPythonFunctions {
    let base = ScriptPythonFunctions {
        names: FUNCTIONS.iter().map(|name| (*name).to_string()).collect(),
        handler: Arc::new(Handler(frame.clone())),
        prelude: None,
    };
    with_tools(base, &frame.tools, Arc::new(Runner(frame.clone())))
}

/// A frame failure ends the run, so a failed call raises with the message the
/// frame recorded.
fn finish(frame: &Frame, result: Result<Value>, host: &ScriptHostContext) -> ExtFunctionResult {
    match result.and_then(to_python) {
        Ok(value) => ExtFunctionResult::Return(value),
        Err(error) => {
            let message = frame.fail(format!("{error:#}"), host);
            ExtFunctionResult::Error(MontyException::new(ExcType::RuntimeError, Some(message)))
        }
    }
}

struct Runner(Arc<Frame>);

#[async_trait::async_trait]
impl PythonToolRunner for Runner {
    async fn run_tool(
        &self,
        name: String,
        arguments: Value,
        host: ScriptHostContext,
    ) -> ExtFunctionResult {
        let result = FrameTools(self.0.clone())
            .execute(&name, arguments, host.cancel.clone())
            .await;
        finish(&self.0, result, &host)
    }
}

struct Handler(Arc<Frame>);

#[async_trait::async_trait]
impl ScriptPythonHandler for Handler {
    async fn call(
        &self,
        name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        host: ScriptHostContext,
    ) -> ExtFunctionResult {
        let result = self.execute(&name, args, kwargs, &host).await;
        finish(&self.0, result, &host)
    }
}

impl Handler {
    async fn execute(
        &self,
        name: &str,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        host: &ScriptHostContext,
    ) -> Result<Value> {
        let mut values = bind(parameters(name)?, args, kwargs)?;
        match name {
            "tool_input" => Ok(self.0.input.clone()),
            "fetch" => {
                self.0
                    .fetch(&string(required(&mut values, "url")?)?, host)
                    .await
            }
            "call_next" => {
                let arguments = values.remove("args").filter(|value| !value.is_null());
                let id = values
                    .remove("id")
                    .filter(|value| !value.is_null())
                    .map(string)
                    .transpose()?;
                self.0.call_next(arguments, id, host).await
            }
            "confirm" => {
                let message = string(required(&mut values, "message")?)?;
                let id = values
                    .remove("id")
                    .filter(|value| !value.is_null())
                    .map(string)
                    .transpose()?;
                self.0.confirm(message, id, host)
            }
            "answer" => self
                .0
                .answer(&string(required(&mut values, "key")?)?)
                .map(|value| value.unwrap_or(Value::Null)),
            "interact" => {
                let message = required(&mut values, "surface")?;
                let id = values
                    .remove("id")
                    .filter(|value| !value.is_null())
                    .map(string)
                    .transpose()?;
                self.0.interact(message, id, host)
            }
            "saved" => self
                .0
                .saved(&string(required(&mut values, "key")?)?)
                .map(|value| value.unwrap_or(Value::Null)),
            "save" => {
                let key = string(required(&mut values, "key")?)?;
                self.0.save(key, required(&mut values, "value")?)
            }
            "respond" => self.0.respond(required(&mut values, "result")?),
            "deny" => self.0.deny(string(required(&mut values, "reason")?)?),
            _ => bail!("unknown middleware function"),
        }
    }
}

fn parameters(name: &str) -> Result<&'static [&'static str]> {
    Ok(match name {
        "tool_input" => &[],
        "fetch" => &["url"],
        "call_next" => &["args", "id"],
        "respond" => &["result"],
        "deny" => &["reason"],
        "confirm" => &["message", "id"],
        "answer" | "saved" => &["key"],
        "save" => &["key", "value"],
        "interact" => &["surface", "id"],
        _ => bail!("unknown middleware function '{name}'"),
    })
}
