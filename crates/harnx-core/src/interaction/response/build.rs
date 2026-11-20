//! Building the canonical `InteractionResponse.data` for an answered surface,
//! so every frontend produces the same `data` for the same form and answer.

use serde_json::{json, Map, Value};

use super::inspect::{data_key, find_action, input_fields, InputKind};

/// Build `data` for an answered surface: every bound input's current value,
/// overlaid with the chosen action's resolved `event.context`.
///
/// `values` maps input keys (the binding pointer minus the leading `/`) to
/// the values the user entered; an absent input falls back to the surface's
/// declared `dataModel` value at its path, then to the kind's empty default
/// (`""` for TextField, `[]` for ChoicePicker). `deny` is dismissal and
/// carries no canonical data: the builder returns an empty object so it
/// matches what a deny-only frontend submits.
///
/// Returns an error when `action` is neither a declared Button event name nor
/// `deny`.
pub fn build_data(
    payload: &Value,
    action: &str,
    values: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    if action == "deny" {
        return Ok(Map::new());
    }
    let fields = input_fields(payload);
    let Some(button) = find_action(payload, action) else {
        return Err(undeclared_action(action));
    };
    let mut data = Map::new();
    for field in &fields {
        let value = values.get(&field.key).cloned().unwrap_or_else(|| {
            default_value(
                payload,
                field.path.as_str(),
                field.kind == InputKind::ChoicePicker,
            )
        });
        data.insert(field.key.clone(), value);
    }
    for (key, entry) in &button.context {
        let resolved = resolve_context_entry(payload, entry, values);
        data.insert(key.clone(), resolved);
    }
    Ok(data)
}

/// A context entry per the pinned semantics: a `{"@path": ...}` binding
/// resolves against the current data model (user input first, then the
/// surface's `dataModel`, then `null`); any other value is a literal and
/// passes through unchanged.
fn resolve_context_entry(payload: &Value, entry: &Value, values: &Map<String, Value>) -> Value {
    let Some(pointer) = entry.get("@path").and_then(Value::as_str) else {
        return entry.clone();
    };
    if let Some(value) = values.get(data_key(pointer)) {
        return value.clone();
    }
    pointer_get(data_model(payload), pointer)
        .cloned()
        .unwrap_or(Value::Null)
}

/// The data model the surface declares, if any.
fn data_model(payload: &Value) -> Option<&Value> {
    payload
        .get("createSurface")
        .and_then(|create| create.get("dataModel"))
}

/// Resolve a JSON pointer into `data_model`, tolerating absent containers.
/// `~0` and `~1` unescape to `~` and `/` per RFC 6901.
pub fn pointer_get<'a>(data_model: Option<&'a Value>, pointer: &str) -> Option<&'a Value> {
    let mut current = data_model?;
    for token in pointer.trim_start_matches('/').split('/') {
        if token.is_empty() {
            continue;
        }
        let token = token.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(object) => object.get(&token)?,
            Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// The default value for an untouched input: the declared `dataModel` value
/// at its path, else the kind's empty default.
fn default_value(payload: &Value, path: &str, choice: bool) -> Value {
    pointer_get(data_model(payload), path)
        .cloned()
        .unwrap_or_else(|| if choice { json!([]) } else { json!("") })
}

/// The declared `dataModel` string at an input's path, for display: nothing
/// when the surface declares none (or a non-string), so callers can fall back
/// to their own placeholder.
pub fn input_default(payload: &Value, path: &str) -> Option<String> {
    pointer_get(data_model(payload), path).and_then(|value| {
        // Only literal strings render as defaults; a binding or structured
        // default shows the caller's placeholder instead.
        value.as_str().map(str::to_string)
    })
}

/// Resolve a display `DynamicString`: a literal or a string at its `@path` in
/// the surface's declared data model. Missing and non-string bindings have no
/// display value; callers choose their own fallback.
pub fn resolve_dynamic_string(payload: &Value, value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(literal) = value.as_str() {
        return Some(literal.to_string());
    }
    input_default(payload, value.get("@path")?.as_str()?)
}

/// Shared error text: acceptance, the builder and the text parser all reject
/// an action the surface never declared.
pub(super) fn undeclared_action(action: &str) -> String {
    format!("action '{action}' is not declared by this surface (and is not 'deny')")
}
