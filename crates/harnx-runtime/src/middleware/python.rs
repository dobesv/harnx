use super::{
    conversion::{from_python, to_python},
    frame::Frame,
    tools::FrameTools,
};
use anyhow::{bail, ensure, Result};
use harnx_script::{
    ExcType, ExtFunctionResult, MontyException, MontyObject, ScriptHostContext,
    ScriptPythonFunctions, ScriptPythonHandler,
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

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
            "call_tool" => {
                let name = string(required(&mut values, "name")?)?;
                let arguments = required(&mut values, "args")?;
                FrameTools(self.0.clone())
                    .execute(&name, arguments, host.cancel.clone())
                    .await
            }
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
        "call_tool" => &["name", "args"],
        "confirm" => &["message", "id"],
        "answer" | "saved" => &["key"],
        "save" => &["key", "value"],
        "interact" => &["surface", "id"],
        _ => bail!("unknown middleware function '{name}'"),
    })
}

fn bind(
    parameters: &[&str],
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> Result<HashMap<String, Value>> {
    ensure!(
        args.len() <= parameters.len(),
        "too many positional middleware arguments"
    );
    let mut values = HashMap::new();
    for (name, value) in parameters.iter().zip(args) {
        values.insert((*name).to_string(), from_python(value)?);
    }
    for (key, value) in kwargs {
        let name = string(from_python(key)?)?;
        ensure!(
            parameters.contains(&name.as_str()),
            "unknown middleware argument '{name}'"
        );
        ensure!(
            !values.contains_key(&name),
            "duplicate middleware argument '{name}'"
        );
        values.insert(name, from_python(value)?);
    }
    Ok(values)
}

fn required(values: &mut HashMap<String, Value>, name: &str) -> Result<Value> {
    values
        .remove(name)
        .ok_or_else(|| anyhow::anyhow!("missing middleware argument '{name}'"))
}

fn string(value: Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value),
        _ => bail!("middleware argument must be a string"),
    }
}
