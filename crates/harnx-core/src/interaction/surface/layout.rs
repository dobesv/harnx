//! Layout components: Text, Button, Card and Column.

use serde_json::{Map, Value};

use super::action::validate_action;
use super::binding::{required_dynamic_field, required_text, validate_enum};
use super::util::property_path;

const TEXT_VARIANTS: &[&str] = &["body", "caption"];
const BUTTON_VARIANTS: &[&str] = &["default", "primary", "borderless"];
const JUSTIFY_VALUES: &[&str] = &[
    "start",
    "center",
    "end",
    "spaceBetween",
    "spaceAround",
    "spaceEvenly",
    "stretch",
];
const ALIGN_VALUES: &[&str] = &["center", "end", "start", "stretch"];

pub(super) fn validate_text(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    required_dynamic_field(object, "text", component_path)?;
    validate_enum(
        object.get("variant"),
        TEXT_VARIANTS,
        &property_path(component_path, "variant"),
    )
}

pub(super) fn validate_button(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    required_text(object, "child", component_path)?;
    validate_enum(
        object.get("variant"),
        BUTTON_VARIANTS,
        &property_path(component_path, "variant"),
    )?;
    required_action(object, component_path)
}

fn required_action(object: &Map<String, Value>, component_path: &str) -> Result<(), String> {
    if let Some(action) = object.get("action") {
        return validate_action(action, &property_path(component_path, "action"));
    }
    Err(format!("{component_path}: missing required field 'action'"))
}

pub(super) fn validate_card(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    required_text(object, "child", component_path)?;
    Ok(())
}

pub(super) fn validate_column(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    required_children(object, component_path)?;
    validate_enum(
        object.get("justify"),
        JUSTIFY_VALUES,
        &property_path(component_path, "justify"),
    )?;
    validate_enum(
        object.get("align"),
        ALIGN_VALUES,
        &property_path(component_path, "align"),
    )
}

/// Column children are either a list of component ids or a
/// `{componentId, path}` template over a data-model list.
fn required_children(object: &Map<String, Value>, component_path: &str) -> Result<(), String> {
    match object.get("children") {
        Some(Value::Array(children)) => validate_child_list(children, component_path),
        Some(Value::Object(template)) => validate_child_template(template, component_path),
        Some(_) => Err(format!(
            "{component_path}.children must be an array of component ids or a {{componentId, \
             path}} template"
        )),
        None => Err(format!(
            "{component_path}: missing required field 'children'"
        )),
    }
}

fn validate_child_list(children: &[Value], component_path: &str) -> Result<(), String> {
    if children.is_empty() {
        return Err(format!("{component_path}.children must not be empty"));
    }
    for (index, child) in children.iter().enumerate() {
        ensure_child_id(child, &format!("{component_path}.children[{index}]"))?;
    }
    Ok(())
}

fn ensure_child_id(child: &Value, child_path: &str) -> Result<(), String> {
    let child = child.as_str().unwrap_or_default();
    if child.is_empty() {
        return Err(format!("{child_path} must be a component id"));
    }
    Ok(())
}

fn validate_child_template(
    template: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    reject_unknown_template_keys(template, component_path)?;
    validate_template_field(template, "componentId", component_path)?;
    validate_template_field(template, "path", component_path)
}

fn reject_unknown_template_keys(
    template: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    for key in template.keys() {
        if key != "componentId" && key != "path" {
            return Err(format!(
                "{component_path}.children: unknown template field '{key}'"
            ));
        }
    }
    Ok(())
}

fn validate_template_field(
    template: &Map<String, Value>,
    field: &str,
    component_path: &str,
) -> Result<(), String> {
    match template.get(field) {
        Some(Value::String(value)) if !value.is_empty() => Ok(()),
        Some(_) => Err(format!(
            "{component_path}.children.{field} must be a non-empty string"
        )),
        None => Err(format!(
            "{component_path}.children: missing required template field '{field}'"
        )),
    }
}
