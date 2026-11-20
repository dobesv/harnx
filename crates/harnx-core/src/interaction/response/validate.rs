//! Acceptance-side validation of an `a2ui` response against the requested
//! surface: the one check every frontend's answer passes through.

use serde_json::Value;

use super::build::undeclared_action;
use super::inspect::{find_action, input_fields, InputField, InputKind};
use crate::interaction::InteractionResponse;

/// Validate a response against the requested surface.
///
/// Rules (see the module docs for the full contract):
/// - `deny` is always accepted, with any `data`: it is dismissal and carries
///   no canonical payload, and deny-only frontends may echo values.
/// - Otherwise the action must name a declared Button `action.event.name`.
/// - Every `data` key must be a bound input's key or a declared context key of
///   the chosen action. Context keys accept any JSON value (a resolved
///   binding carries user-controlled data); input keys are type-checked:
///   TextField values are strings, ChoicePicker values are arrays of
///   declared option values, at most one when the variant is exclusive.
/// - Missing keys are allowed: a renderer may omit untouched inputs.
pub fn validate_response(payload: &Value, response: &InteractionResponse) -> Result<(), String> {
    if response.action == "deny" {
        return Ok(());
    }
    let Some(button) = find_action(payload, &response.action) else {
        return Err(undeclared_action(&response.action));
    };
    let fields = input_fields(payload);
    for (key, value) in &response.data {
        match fields.iter().find(|field| field.key == *key) {
            Some(field) => check_input_value(field, value)?,
            None if button.context.contains_key(key) => {}
            None => return Err(unknown_data_key(key)),
        }
    }
    Ok(())
}

/// Type-check one bound input's value.
fn check_input_value(field: &InputField, value: &Value) -> Result<(), String> {
    match field.kind {
        InputKind::TextField => value.as_str().map_or_else(
            || Err(format!("data '{}' must be a string", field.key)),
            |_| Ok(()),
        ),
        InputKind::ChoicePicker => check_choice_value(field, value),
    }
}

fn check_choice_value(field: &InputField, value: &Value) -> Result<(), String> {
    let items = value.as_array().ok_or_else(|| {
        format!(
            "data '{}' must be an array of selected option values",
            field.key
        )
    })?;
    if field.exclusive && items.len() > 1 {
        return Err(format!(
            "data '{}' selects {} values but the picker is mutually exclusive",
            field.key,
            items.len()
        ));
    }
    for item in items {
        let selected = item
            .as_str()
            .ok_or_else(|| format!("data '{}' must contain option value strings", field.key))?;
        if !field.options.iter().any(|option| option.value == selected) {
            return Err(format!(
                "data '{}' selects undeclared option value '{selected}'",
                field.key
            ));
        }
    }
    Ok(())
}

fn unknown_data_key(key: &str) -> String {
    format!("data key '{key}' matches no input binding or action context entry of this surface")
}
