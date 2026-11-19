//! Shared JSON-shape helpers used by every validation stage.

use serde_json::{Map, Value};

/// Expects a JSON object. `path` may embed a subject, so
/// `expect_object(component, "components[0]: component")` reports
/// "components[0]: component must be a JSON object".
pub(super) fn expect_object<'a>(
    value: &'a Value,
    path: &str,
) -> Result<&'a Map<String, Value>, String> {
    let Some(object) = value.as_object() else {
        return Err(format!("{path} must be a JSON object"));
    };
    Ok(object)
}

/// Rejects keys outside `allowed`; `kind` names them in the error
/// ("unknown field", "unknown property", ...).
pub(super) fn reject_unknown_keys(
    object: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
    kind: &str,
) -> Result<(), String> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{path}: unknown {kind} '{key}'"));
        }
    }
    Ok(())
}

/// The envelope version is pinned to the A2UI release this validator tracks.
pub(super) fn expect_version(object: &Map<String, Value>) -> Result<(), String> {
    match object.get("version") {
        Some(Value::String(version)) if version == "1.0" => Ok(()),
        Some(other) => Err(format!(
            "version must be \"1.0\", got {}",
            serde_json::to_string(other).unwrap_or_default()
        )),
        None => Err("missing required field 'version'".into()),
    }
}

pub(super) fn ensure_non_empty_string(value: &Value, path: &str) -> Result<(), String> {
    match value {
        Value::String(text) if !text.is_empty() => Ok(()),
        _ => Err(format!("{path} must be a non-empty string")),
    }
}

/// Path of a component's property, e.g. `submit.variant` — the shared form
/// behind every property error message.
pub(super) fn property_path(component_path: &str, property: &str) -> String {
    format!("{component_path}.{property}")
}

/// The shared "missing required field" error.
pub(super) fn missing_field(path: &str, name: &str) -> String {
    format!("{path}: missing required field '{name}'")
}

/// `metadata` and `@metadata` follow the A2UI `Metadata` shape: only
/// `extensions`, keyed by Unicode-style identifiers.
pub(super) fn validate_metadata(metadata: &Value, path: &str) -> Result<(), String> {
    let object = expect_object(metadata, path)?;
    reject_unknown_keys(object, &["extensions"], path, "field")?;
    validate_extension_keys(object, path)
}

fn validate_extension_keys(metadata: &Map<String, Value>, path: &str) -> Result<(), String> {
    if let Some(extensions) = metadata.get("extensions") {
        return validate_extensions(extensions, path);
    }
    Ok(())
}

fn validate_extensions(extensions: &Value, path: &str) -> Result<(), String> {
    let extensions = expect_object(extensions, &format!("{path}.extensions"))?;
    for key in extensions.keys() {
        if !is_identifier(key) {
            return Err(format!(
                "{path}.extensions: key '{key}' must be a Unicode-style identifier"
            ));
        }
    }
    Ok(())
}

/// Approximation of the `Extensions` key pattern
/// (`^[\p{XID_Start}_][\p{XID_Continue}]*$`): ASCII identifiers. All harnx
/// and A2UI extension keys in the wild are ASCII.
fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}
