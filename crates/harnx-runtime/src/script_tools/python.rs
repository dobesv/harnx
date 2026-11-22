//! Shared Python argument binding for script tool calls and middleware operations.

use super::conversion::from_python;
use anyhow::{bail, ensure, Result};
use harnx_script::MontyObject;
use serde_json::Value;
use std::collections::HashMap;

pub(crate) fn bind(
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

pub(crate) fn required(values: &mut HashMap<String, Value>, name: &str) -> Result<Value> {
    values
        .remove(name)
        .ok_or_else(|| anyhow::anyhow!("missing middleware argument '{name}'"))
}

pub(crate) fn string(value: Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value),
        _ => bail!("middleware argument must be a string"),
    }
}

/// Keep the hook and middleware `call_tool(name, args)` contracts identical.
pub(crate) fn tool_arguments(
    args: Vec<MontyObject>,
    kwargs: Vec<(MontyObject, MontyObject)>,
) -> Result<(String, Value)> {
    let mut values = bind(&["name", "args"], args, kwargs)?;
    let name = string(required(&mut values, "name")?)?;
    let arguments = required(&mut values, "args")?;
    Ok((name, arguments))
}
