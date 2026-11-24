//! Per-component validation: common `ComponentCommon` fields, then
//! dispatch to the per-kind validators in `input` and `layout`.

use serde_json::{Map, Value};

use super::binding::{required_text, validate_binding};
use super::input::{validate_choice_picker, validate_text_field};
use super::layout::{validate_button, validate_card, validate_column, validate_text};
use super::util::{
    ensure_non_empty_string, expect_object, property_path, reject_unknown_keys, validate_metadata,
};
use super::SUPPORTED_COMPONENTS;

pub(super) fn validate_component<'a>(component: &'a Value, path: &str) -> Result<&'a str, String> {
    let object = expect_object(component, &format!("{path}: component"))?;
    let id = required_text(object, "id", path)?;
    let kind = required_text(object, "component", &format!("{path} (id '{id}')"))?;
    ensure_supported(kind, path, id)?;
    let component_path = format!("{path} (id '{id}')");
    reject_unknown_properties(object, kind, &component_path)?;
    validate_common_fields(object, &component_path)?;
    validate_kind(object, kind, &component_path)?;
    Ok(id)
}

/// The implicit Surface container is not a definable component, and only
/// the six pinned Basic Catalog kinds render.
fn ensure_supported(kind: &str, path: &str, id: &str) -> Result<(), String> {
    if kind == "Surface" {
        return Err(format!(
            "{path} (id '{id}'): component 'Surface' is reserved and cannot be defined"
        ));
    }
    if !SUPPORTED_COMPONENTS.contains(&kind) {
        return Err(format!(
            "{path} (id '{id}'): component '{kind}' is not in the supported Basic Catalog subset \
             (Text, Button, ChoicePicker, TextField, Card, Column)"
        ));
    }
    Ok(())
}

fn reject_unknown_properties(
    object: &Map<String, Value>,
    kind: &str,
    component_path: &str,
) -> Result<(), String> {
    let checkable = matches!(kind, "Button" | "ChoicePicker" | "TextField");
    for key in object.keys() {
        if !is_allowed_property(key, kind, checkable) {
            return Err(format!(
                "{component_path}: unknown property '{key}' for component '{kind}'"
            ));
        }
    }
    Ok(())
}

/// Per-kind property allowlist; the catalog's own fields plus the subset's
/// `value`/`label`/`@checks` extensions. `@checks` is `Checkable` and only
/// input components are checkable.
fn is_allowed_property(key: &str, kind: &str, checkable: bool) -> bool {
    match key {
        "id" | "component" | "weight" | "@catalogId" | "@accessibility" | "@metadata"
        | "metadata" => true,
        "@checks" => checkable,
        other => matches!(
            (kind, other),
            ("Text", "text" | "variant")
                | ("Button", "child" | "action" | "variant")
                | (
                    "ChoicePicker",
                    "label" | "variant" | "options" | "value" | "displayStyle" | "filterable"
                )
                | ("TextField", "label" | "value" | "placeholder" | "variant")
                | ("Card", "child")
                | ("Column", "children" | "justify" | "align")
        ),
    }
}

fn validate_common_fields(object: &Map<String, Value>, component_path: &str) -> Result<(), String> {
    validate_catalog_id(object, component_path)?;
    validate_component_metadata(object, component_path)?;
    validate_spec_component_metadata(object, component_path)?;
    validate_accessibility_field(object, component_path)?;
    validate_weight(object, component_path)?;
    validate_checks(object, component_path)
}

fn validate_catalog_id(object: &Map<String, Value>, component_path: &str) -> Result<(), String> {
    if let Some(catalog) = object.get("@catalogId") {
        return ensure_non_empty_string(catalog, &property_path(component_path, "@catalogId"));
    }
    Ok(())
}

fn validate_component_metadata(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    if let Some(metadata) = object.get("@metadata") {
        return validate_metadata(metadata, &property_path(component_path, "@metadata"));
    }
    Ok(())
}

/// Component-level `metadata` — the A2UI spec's per-component Metadata slot,
/// distinct from the catalog's `@metadata` field. Same shape, same validation.
/// Plan T5.1 carries `harnx.visibility` / `harnx.tool` here.
fn validate_spec_component_metadata(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    if let Some(metadata) = object.get("metadata") {
        return validate_metadata(metadata, &property_path(component_path, "metadata"));
    }
    Ok(())
}

fn validate_accessibility_field(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    if let Some(accessibility) = object.get("@accessibility") {
        return super::binding::validate_accessibility(
            accessibility,
            &property_path(component_path, "@accessibility"),
        );
    }
    Ok(())
}

fn validate_weight(object: &Map<String, Value>, component_path: &str) -> Result<(), String> {
    if let Some(weight) = object.get("weight") {
        return ensure_number(weight, component_path);
    }
    Ok(())
}

fn ensure_number(value: &Value, component_path: &str) -> Result<(), String> {
    if !value.is_number() {
        return Err(format!("{}.weight must be a number", component_path));
    }
    Ok(())
}

/// `@checks` conditions are data bindings so a frontend can evaluate them
/// against the data model without a renderer function call.
fn validate_checks(object: &Map<String, Value>, component_path: &str) -> Result<(), String> {
    if let Some(checks) = object.get("@checks") {
        return validate_check_list(checks, component_path);
    }
    Ok(())
}

fn validate_check_list(checks: &Value, component_path: &str) -> Result<(), String> {
    let Some(list) = checks.as_array() else {
        return Err(format!("{}.@checks must be an array", component_path));
    };
    for (index, check) in list.iter().enumerate() {
        validate_check(check, &format!("{component_path}.@checks[{index}]"))?;
    }
    Ok(())
}

fn validate_check(check: &Value, check_path: &str) -> Result<(), String> {
    let Some(check) = check.as_object() else {
        return Err(format!("{check_path}: check must be a JSON object"));
    };
    reject_unknown_keys(check, &["condition", "message"], check_path, "field")?;
    validate_check_message(check, check_path)?;
    validate_check_condition(check, check_path)
}

fn validate_check_message(check: &Map<String, Value>, check_path: &str) -> Result<(), String> {
    if let Some(message) = check.get("message") {
        return ensure_string(message, &property_path(check_path, "message"));
    }
    Ok(())
}

fn ensure_string(value: &Value, path: &str) -> Result<(), String> {
    if !value.is_string() {
        return Err(format!("{path} must be a string"));
    }
    Ok(())
}

fn validate_check_condition(check: &Map<String, Value>, check_path: &str) -> Result<(), String> {
    if let Some(condition) = check.get("condition") {
        return validate_binding(condition, &property_path(check_path, "condition"));
    }
    Err(format!("{check_path}: missing required field 'condition'"))
}

/// Dispatch to the per-kind validators; kinds outside the subset were
/// rejected by `ensure_supported`.
fn validate_kind(
    object: &Map<String, Value>,
    kind: &str,
    component_path: &str,
) -> Result<(), String> {
    match kind {
        "Text" => validate_text(object, component_path),
        "Button" => validate_button(object, component_path),
        "ChoicePicker" => validate_choice_picker(object, component_path),
        "TextField" => validate_text_field(object, component_path),
        "Card" => validate_card(object, component_path),
        "Column" => validate_column(object, component_path),
        _ => Ok(()),
    }
}
