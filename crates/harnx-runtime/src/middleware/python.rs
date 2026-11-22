use super::{frame::Frame, tools::FrameTools};
use crate::script_tools::{
    conversion::to_python,
    python::{bind, required, string, tool_arguments},
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
    "call_tool",
    "confirm",
    "answer",
    "save",
    "saved",
    "interact",
];

pub(super) fn functions(frame: &Arc<Frame>) -> ScriptPythonFunctions {
    ScriptPythonFunctions {
        names: FUNCTIONS.iter().map(|name| (*name).to_string()).collect(),
        handler: Arc::new(Handler(frame.clone())),
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
        match self
            .execute(&name, args, kwargs, &host)
            .await
            .and_then(to_python)
        {
            Ok(value) => ExtFunctionResult::Return(value),
            Err(error) => {
                let message = self.0.fail(format!("{error:#}"), &host);
                ExtFunctionResult::Error(MontyException::new(ExcType::RuntimeError, Some(message)))
            }
        }
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
        if name == "call_tool" {
            let (name, arguments) = tool_arguments(args, kwargs)?;
            return FrameTools(self.0.clone())
                .execute(&name, arguments, host.cancel.clone())
                .await;
        }
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
