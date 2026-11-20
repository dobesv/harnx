//! Button `action` validation: agent-side `Action` events only, since the
//! runtime implements no renderer function calls.

use serde_json::{Map, Value};

use super::binding::{validate_dynamic_string, validate_dynamic_value};
use super::util::{expect_object, property_path, reject_unknown_keys};

pub(super) fn validate_action(action: &Value, path: &str) -> Result<(), String> {
    let object = expect_object(action, &format!("{path}: action"))?;
    reject_unknown_keys(object, &["event", "functionCall"], path, "field")?;
    require_event(object, path)?;
    let event = expect_event(object, path)?;
    validate_event(event, &format!("{path}.event"))
}

/// `functionCall` actions run renderer functions the harnx runtime does not
/// implement, so `event` is required.
fn require_event(object: &Map<String, Value>, path: &str) -> Result<(), String> {
    if !object.contains_key("event") {
        return Err(format!(
            "{path}: renderer function calls are not supported in interact surfaces; use \
             action.event"
        ));
    }
    Ok(())
}

fn expect_event<'a>(
    object: &'a Map<String, Value>,
    path: &str,
) -> Result<&'a Map<String, Value>, String> {
    object
        .get("event")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{path}.event: must be a JSON object"))
}

fn validate_event(event: &Map<String, Value>, event_path: &str) -> Result<(), String> {
    reject_unknown_keys(
        event,
        &["name", "userMessage", "context"],
        event_path,
        "field",
    )?;
    validate_event_name(event, event_path)?;
    validate_user_message(event, event_path)?;
    validate_context(event, event_path)
}

/// The event name is how an accepted action names this Button.
fn validate_event_name(event: &Map<String, Value>, event_path: &str) -> Result<(), String> {
    match event.get("name") {
        Some(Value::String(name)) if !name.is_empty() => Ok(()),
        Some(_) => Err(format!("{event_path}.name must be a non-empty string")),
        None => Err(format!("{event_path}: missing required field 'name'")),
    }
}

fn validate_user_message(event: &Map<String, Value>, event_path: &str) -> Result<(), String> {
    if let Some(user_message) = event.get("userMessage") {
        return validate_dynamic_string(user_message, &property_path(event_path, "userMessage"));
    }
    Ok(())
}

fn validate_context(event: &Map<String, Value>, event_path: &str) -> Result<(), String> {
    if let Some(context) = event.get("context") {
        return validate_context_object(context, event_path);
    }
    Ok(())
}

fn validate_context_object(context: &Value, event_path: &str) -> Result<(), String> {
    let context_path = format!("{event_path}.context");
    let context = expect_object(context, &format!("{context_path}:"))?;
    for (key, value) in context {
        validate_dynamic_value(value, &property_path(&context_path, key))?;
    }
    Ok(())
}
