//! The pure parts of a script's `tools` object: the Python that builds it,
//! the binding of a call's arguments to a tool's input schema, and the value
//! a call returns.

use serde_json::{Map, Value};

use crate::{cli_map::validation::validate, commands::render, schema::accepts_null};

/// The host function every `tools.NAME(...)` call forwards to.
pub const PYTHON_CALL_FUNCTION: &str = "_harnx_call";

/// The message of the `TypeError` a bad call raises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PythonCallError(pub String);

/// The Python source that builds `tools`: two lines whatever the tool count.
pub fn tools_prelude(tool_names: &[String]) -> String {
    let quoted: Vec<String> = tool_names
        .iter()
        .map(|name| Value::String(name.clone()).to_string())
        .collect();
    let names = format!("[{}]", quoted.join(", "));
    format!(
        "class tools: pass\n\
         _harnx_tool = lambda n: (lambda *a, **k: {PYTHON_CALL_FUNCTION}(n, list(a), k)); \
         tools = tools(); [setattr(tools, n, _harnx_tool(n)) for n in {names}]"
    )
}

/// A `tools.NAME(...)` call's arguments as the tool's JSON arguments, in the
/// order the schema lists its properties; a keyword that is not a listed
/// property is passed through after them only when the schema is open (its
/// top-level `additionalProperties` is `true` or an object, or it has neither
/// that key nor `properties`), and is a `TypeError` otherwise. A name in
/// `required` that `properties` does not list is a required keyword with no
/// positional slot, as it is for the Bash flags.
///
/// `None` for an optional argument is left out unless the argument's schema
/// accepts null, which is how a tool says a null means something. Values are
/// checked for their direct type and `enum` only; a tool applies the rest of
/// its schema itself.
pub fn bind_python_call(
    tool: &str,
    input_schema: &Value,
    args: Vec<Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<Value, PythonCallError> {
    let fail = |message: String| PythonCallError(format!("{tool}() {message}"));
    let mut bound = place_positionals(&required_keys(input_schema), args).map_err(fail)?;
    place_keywords(input_schema, &mut bound, kwargs).map_err(fail)?;
    let required = all_required(input_schema);
    check_required(&required, &bound).map_err(fail)?;
    drop_optional_nulls(input_schema, &required, &mut bound);
    validate_values(input_schema, &bound).map_err(fail)?;
    Ok(in_schema_order(input_schema, bound))
}

/// What a call returns in the `bash` tool: what the tool's command prints
/// with `-o json`. That is the result's `structuredContent` whatever its
/// type; a result that is not an object with a `content` array (a native tool
/// can return a plain object) as it is; otherwise the text the command prints.
pub fn python_value(tool_output: &Value) -> Value {
    if let Some(structured) = tool_output.get("structuredContent") {
        return structured.clone();
    }
    let has_content = tool_output.get("content").is_some_and(Value::is_array);
    if tool_output.is_object() && !has_content {
        return tool_output.clone();
    }
    Value::String(render::output(tool_output, None))
}

pub(crate) fn property_keys(schema: &Value) -> Vec<&str> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// The required property keys, in the order `properties` lists them.
pub(crate) fn required_keys(schema: &Value) -> Vec<&str> {
    let required = schema.get("required").and_then(Value::as_array);
    property_keys(schema)
        .into_iter()
        .filter(|key| required.is_some_and(|items| items.iter().any(|i| i.as_str() == Some(key))))
        .collect()
}

/// Every required name: the listed ones in `properties` order, then the ones
/// `properties` does not list, in the order `required` gives them.
fn all_required(schema: &Value) -> Vec<&str> {
    let mut all = required_keys(schema);
    let listed = property_keys(schema);
    let unlisted = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| !listed.contains(name));
    for name in unlisted {
        if !all.contains(&name) {
            all.push(name);
        }
    }
    all
}

fn is_open(schema: &Value) -> bool {
    match schema.get("additionalProperties") {
        Some(Value::Bool(open)) => *open,
        Some(Value::Object(_)) => true,
        _ => schema.get("properties").is_none(),
    }
}

/// The schema an argument's value has to fit: its own property's, or for a
/// free-form keyword the one `additionalProperties` gives, when it gives one.
fn value_schema<'a>(schema: &'a Value, key: &str) -> Option<&'a Value> {
    let listed = schema.get("properties").and_then(|p| p.get(key));
    listed.or_else(|| {
        schema
            .get("additionalProperties")
            .filter(|extra| extra.is_object())
    })
}

fn place_positionals(required: &[&str], args: Vec<Value>) -> Result<Map<String, Value>, String> {
    if args.len() > required.len() {
        let noun = if required.len() == 1 {
            "argument"
        } else {
            "arguments"
        };
        return Err(format!(
            "takes at most {} positional {noun} but {} were given",
            required.len(),
            args.len()
        ));
    }
    Ok(required
        .iter()
        .zip(args)
        .map(|(key, value)| ((*key).to_owned(), value))
        .collect())
}

fn place_keywords(
    schema: &Value,
    bound: &mut Map<String, Value>,
    kwargs: Vec<(String, Value)>,
) -> Result<(), String> {
    let known = property_keys(schema);
    let required = all_required(schema);
    let open = is_open(schema);
    for (key, value) in kwargs {
        if !known.contains(&key.as_str()) && !required.contains(&key.as_str()) && !open {
            return Err(format!("got an unexpected keyword argument '{key}'"));
        }
        if bound.contains_key(&key) {
            return Err(format!("got multiple values for argument '{key}'"));
        }
        bound.insert(key, value);
    }
    Ok(())
}

fn check_required(required: &[&str], bound: &Map<String, Value>) -> Result<(), String> {
    let missing: Vec<String> = required
        .iter()
        .filter(|key| !bound.contains_key(**key))
        .map(|key| format!("'{key}'"))
        .collect();
    match missing.as_slice() {
        [] => Ok(()),
        [only] => Err(format!("missing 1 required argument: {only}")),
        [init @ .., last] => Err(format!(
            "missing {} required arguments: {} and {last}",
            missing.len(),
            init.join(", ")
        )),
    }
}

fn drop_optional_nulls(schema: &Value, required: &[&str], bound: &mut Map<String, Value>) {
    bound.retain(|key, value| {
        !value.is_null()
            || required.contains(&key.as_str())
            || value_schema(schema, key).is_some_and(accepts_null)
    });
}

fn validate_values(schema: &Value, bound: &Map<String, Value>) -> Result<(), String> {
    for (key, value) in bound {
        let Some(property) = value_schema(schema, key) else {
            continue;
        };
        if value.is_null() && accepts_null(property) {
            continue;
        }
        validate(property, value).map_err(|complaint| format!("argument '{key}': {complaint}"))?;
    }
    Ok(())
}

fn in_schema_order(schema: &Value, mut bound: Map<String, Value>) -> Value {
    let mut ordered = Map::new();
    for key in property_keys(schema) {
        if let Some(value) = bound.remove(key) {
            ordered.insert(key.to_owned(), value);
        }
    }
    // What is left are free-form keywords, in the order the caller gave them.
    ordered.extend(bound);
    Value::Object(ordered)
}

#[cfg(test)]
mod tests;
