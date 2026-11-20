//! Data bindings and dynamic values: `DynamicString`, `DynamicValue` and
//! `DataBinding` from the pinned `common_types.json`, plus the per-field
//! helpers the component validators build on.

use serde_json::{Map, Value};

use super::util::{missing_field, property_path};

/// `DynamicString`: a string literal or a data-model binding.
pub(super) fn validate_dynamic_string(value: &Value, path: &str) -> Result<(), String> {
    match value {
        Value::String(_) => Ok(()),
        Value::Object(binding) => validate_binding_object(binding, path),
        _ => Err(format!("{path} must be a string or a data-model binding")),
    }
}

/// `DynamicValue`: a literal, a data-model binding, or a literal
/// object/array of values. Function calls are rejected in this subset.
pub(super) fn validate_dynamic_value(value: &Value, path: &str) -> Result<(), String> {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => Ok(()),
        Value::Array(items) => validate_dynamic_array(items, path),
        Value::Object(object) => validate_dynamic_object(object, path),
    }
}

fn validate_dynamic_array(items: &[Value], path: &str) -> Result<(), String> {
    for (index, item) in items.iter().enumerate() {
        validate_dynamic_value(item, &format!("{path}[{index}]"))?;
    }
    Ok(())
}

fn validate_dynamic_object(object: &Map<String, Value>, path: &str) -> Result<(), String> {
    if is_binding(object) {
        return validate_binding_object(object, path);
    }
    validate_literal_entries(object, path)
}

/// An object carrying a binding marker: `@path` (this subset) or `@call`
/// (rejected with a binding-specific message).
fn is_binding(object: &Map<String, Value>) -> bool {
    object.contains_key("@path") || object.contains_key("@call")
}

fn validate_literal_entries(object: &Map<String, Value>, path: &str) -> Result<(), String> {
    for (key, nested) in object {
        if key.starts_with('@') {
            return Err(format!(
                "{path}.{key}: keys of literal objects must not start with '@'; use \
                 {{\"@path\": ...}} for bindings"
            ));
        }
        validate_dynamic_value(nested, &format!("{path}.{key}"))?;
    }
    Ok(())
}

/// Input components must bind their value to a data-model path, so an
/// answered action can carry the user's input back to the script.
pub(super) fn validate_input_binding(value: &Value, path: &str) -> Result<(), String> {
    let Value::Object(binding) = value else {
        return Err(format!(
            "{path} must be a data-model binding like {{\"@path\": \"/choice\"}} so the \
             response can carry the user's input back"
        ));
    };
    validate_binding_object(binding, path)
}

/// `DataBinding` in `@checks[].condition`: exactly
/// `{"@path": "<json pointer>"}`. `{ "@call": ... }` function calls are
/// rejected because the runtime implements no catalog functions.
pub(super) fn validate_binding(binding: &Value, path: &str) -> Result<(), String> {
    let Value::Object(binding) = binding else {
        return Err(format!(
            "{path} must be a data-model binding like {{\"@path\": \"/value\"}}"
        ));
    };
    validate_binding_object(binding, path)
}

/// `DataBinding`: exactly `{ "@path": "<json pointer>" }` — no other keys.
/// `{ "@call": ... }` function calls are rejected because the runtime
/// implements no catalog functions, and unexpected extra keys are rejected
/// so stale or misspelled binding options fail loudly instead of being
/// silently ignored.
fn validate_binding_object(binding: &Map<String, Value>, path: &str) -> Result<(), String> {
    if binding.contains_key("@call") {
        return Err(format!(
            "{path}: function calls ({{\"@call\": ...}}) are not supported in interact surfaces"
        ));
    }
    match binding.get("@path") {
        Some(Value::String(pointer)) if pointer.is_empty() || pointer.starts_with('/') => {}
        Some(_) => {
            return Err(format!(
                "{path}: @path must be a JSON pointer like \"/choice\" (starting with '/')"
            ))
        }
        None => {
            return Err(format!(
                "{path} must be a data-model binding like {{\"@path\": \"/choice\"}} (function \
                 calls are not supported)"
            ))
        }
    }
    if let Some(unexpected) = binding.keys().find(|key| key.as_str() != "@path") {
        return Err(format!(
            "{path}: unexpected key '{unexpected}' in data binding; the binding must be exactly \
             {{\"@path\": \"...\"}}"
        ));
    }
    Ok(())
}

/// An enum-valued property: absent/null keeps the catalog default, any
/// other value must name one of `allowed`.
pub(super) fn validate_enum(
    value: Option<&Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), String> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(actual)) if allowed.contains(&actual.as_str()) => Ok(()),
        Some(Value::String(other)) => Err(format!(
            "{path} must be one of {}, got '{other}'",
            allowed
                .iter()
                .map(|v| format!("'{v}'"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
        Some(_) => Err(format!("{path} must be one of {}", allowed.join(", "))),
    }
}

/// `@accessibility` follows the A2UI `Accessibility` shape: label and
/// description are `DynamicString`s, `live` an enum, `hidden` a boolean or
/// a binding.
pub(super) fn validate_accessibility(accessibility: &Value, path: &str) -> Result<(), String> {
    let Some(object) = accessibility.as_object() else {
        return Err(format!("{path} must be a JSON object"));
    };
    for (key, value) in object {
        let field_path = property_path(path, key);
        match key.as_str() {
            "label" | "description" => validate_dynamic_string(value, &field_path)?,
            "live" => validate_enum(Some(value), &["off", "polite", "assertive"], &field_path)?,
            "hidden" => match value {
                Value::Bool(_) => {}
                Value::Object(binding) => validate_binding_object(binding, &field_path)?,
                _ => return Err(format!("{field_path} must be a boolean or a binding")),
            },
            other => return Err(format!("{path}: unknown field '{other}'")),
        }
    }
    Ok(())
}

/// Required non-empty string field, e.g. a component `id` or a Button
/// `child`.
pub(super) fn required_text<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    path: &str,
) -> Result<&'a str, String> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| missing_field(path, name))
}

/// Required field whose value is a `DynamicString`, e.g. a Text `text`.
pub(super) fn required_dynamic_field(
    object: &Map<String, Value>,
    name: &str,
    path: &str,
) -> Result<(), String> {
    if let Some(value) = object.get(name) {
        return validate_dynamic_string(value, &property_path(path, name));
    }
    Err(missing_field(path, name))
}

/// Optional field whose value is a `DynamicString`, e.g. a TextField
/// `placeholder`.
pub(super) fn optional_dynamic_field(
    object: &Map<String, Value>,
    name: &str,
    path: &str,
) -> Result<(), String> {
    if let Some(value) = object.get(name) {
        return validate_dynamic_string(value, &property_path(path, name));
    }
    Ok(())
}

/// Required input value bound to a data-model path, e.g. a ChoicePicker or
/// TextField `value`.
pub(super) fn required_input_binding(
    object: &Map<String, Value>,
    name: &str,
    path: &str,
) -> Result<(), String> {
    if let Some(value) = object.get(name) {
        return validate_input_binding(value, &property_path(path, name));
    }
    Err(missing_field(path, name))
}
