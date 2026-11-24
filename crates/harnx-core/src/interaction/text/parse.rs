//! Parsing a one-line text answer into the canonical response.

use serde_json::{json, Map, Value};

use crate::interaction::response::{
    build_data, button_actions, input_fields, ButtonAction, InputKind, OptionItem,
};
use crate::interaction::InteractionResponse;

/// Parse a one-line answer against the surface: the first token names the
/// action, the rest are `key=value` pairs (see the module docs for the
/// grammar). The result's `data` is the canonical mapping, so a text answer
/// and a graphical answer to the same form produce the same response.
///
/// `note` is left `None`; callers that carried an operator remark set it
/// afterwards.
pub fn parse(payload: &Value, answer: &str) -> Result<InteractionResponse, String> {
    let tokens: Vec<&str> = answer.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return Err("empty answer: name an action, e.g. `deploy` or `deny`".to_string());
    };
    let action = resolve_action(payload, first)?;
    let mut values = Map::new();
    for token in &tokens[1..] {
        parse_pair(payload, token, &mut values)?;
    }
    let data = build_data(payload, &action, &values)?;
    Ok(InteractionResponse {
        action,
        data,
        note: None,
    })
}

/// The action the first token names: an event name, a button label, or
/// `deny`. Names win over labels so two buttons sharing a label stay
/// addressable; ambiguity between labels is an error.
fn resolve_action(payload: &Value, token: &str) -> Result<String, String> {
    let actions = button_actions(payload);
    if let Some(action) = actions.iter().find(|action| action.name == token) {
        return Ok(action.name.clone());
    }
    let labeled: Vec<&_> = actions
        .iter()
        .filter(|action| action.label.as_deref() == Some(token))
        .collect();
    match labeled.as_slice() {
        [action] => Ok(action.name.clone()),
        [] => {
            if token == "deny" {
                return Ok("deny".to_string());
            }
            Err(format!(
                "'{token}' names no action; valid names: {}",
                action_names(&actions)
            ))
        }
        many => Err(format!(
            "'{token}' matches {} button labels; use the action name instead",
            many.len()
        )),
    }
}

fn action_names(actions: &[ButtonAction]) -> String {
    let mut names = actions
        .iter()
        .map(|action| action.name.as_str())
        .collect::<Vec<_>>();
    names.push("deny");
    names.join(", ")
}

/// One `key=value` pair into `values`, keyed by the input's data key. The
/// key may be the bare key or the full pointer (`/name=Ada`). A ChoicePicker
/// value is a comma-separated list of option values or labels; a TextField
/// value is a single token.
fn parse_pair(payload: &Value, token: &str, values: &mut Map<String, Value>) -> Result<(), String> {
    let Some((key, value)) = token.split_once('=') else {
        return Err(format!("expected key=value, got '{token}'"));
    };
    let key = key.strip_prefix('/').unwrap_or(key);
    let Some(field) = input_fields(payload)
        .into_iter()
        .find(|field| field.key == key)
    else {
        return Err(format!("unknown input key '{key}'"));
    };
    let value = match field.kind {
        InputKind::TextField => json!(value),
        InputKind::ChoicePicker => choice_value(&field.options, value)?,
    };
    values.insert(field.key.clone(), value);
    Ok(())
}

/// Each comma-separated item must be a declared option value, or a label that
/// maps to exactly one value.
fn choice_value(options: &[OptionItem], value: &str) -> Result<Value, String> {
    let mut selected = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if let Some(value) = options
            .iter()
            .find(|option| option.value == item)
            .map(|option| option.value.clone())
        {
            selected.push(json!(value));
            continue;
        }
        let by_label: Vec<_> = options
            .iter()
            .filter(|option| option.label == item)
            .map(|option| option.value.clone())
            .collect();
        match by_label.as_slice() {
            [one] => selected.push(json!(one)),
            [] => return Err(format!("'{item}' is not a declared option value or label")),
            _ => return Err(format!("option label '{item}' is ambiguous; use the value")),
        }
    }
    Ok(json!(selected))
}
