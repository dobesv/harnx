//! `createSurface` structure: allowed fields, the components list, id
//! uniqueness, the root container, child references and answerability.

use serde_json::{Map, Value};

use super::component::validate_component;
use super::util::{
    ensure_non_empty_string, expect_object, property_path, reject_unknown_keys, validate_metadata,
};

const CREATE_FIELDS: &[&str] = &[
    "surfaceId",
    "catalogId",
    "sendDataModel",
    "components",
    "dataModel",
    "metadata",
];

pub(super) fn validate_create_surface(create: &Value) -> Result<(), String> {
    let object = expect_object(create, "createSurface")?;
    reject_unknown_keys(object, CREATE_FIELDS, "createSurface", "field")?;
    validate_optional_fields(object)?;
    let list = components_list(object)?;
    let ids = component_ids(list)?;
    reject_duplicate_ids(&ids)?;
    require_root(&ids)?;
    resolve_child_references(list, &ids)?;
    super::graph::validate_component_graph(list)?;
    require_answerable(list)
}

fn validate_optional_fields(object: &Map<String, Value>) -> Result<(), String> {
    validate_optional_string(object, "surfaceId", "createSurface.surfaceId")?;
    validate_optional_string(object, "catalogId", "createSurface.catalogId")?;
    validate_send_data_model(object)?;
    validate_data_model(object)?;
    validate_metadata_field(object)
}

fn validate_optional_string(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), String> {
    if let Some(value) = object.get(field) {
        return ensure_non_empty_string(value, path);
    }
    Ok(())
}

fn validate_send_data_model(object: &Map<String, Value>) -> Result<(), String> {
    if let Some(send) = object.get("sendDataModel") {
        return ensure_boolean(send, "createSurface.sendDataModel");
    }
    Ok(())
}

fn ensure_boolean(value: &Value, path: &str) -> Result<(), String> {
    if !value.is_boolean() {
        return Err(format!("{path} must be a boolean"));
    }
    Ok(())
}

fn validate_data_model(object: &Map<String, Value>) -> Result<(), String> {
    if let Some(data_model) = object.get("dataModel") {
        return ensure_object(data_model, "createSurface.dataModel");
    }
    Ok(())
}

fn ensure_object(value: &Value, path: &str) -> Result<(), String> {
    if !value.is_object() {
        return Err(format!("{path} must be a JSON object"));
    }
    Ok(())
}

fn validate_metadata_field(object: &Map<String, Value>) -> Result<(), String> {
    if let Some(metadata) = object.get("metadata") {
        return validate_metadata(metadata, &property_path("createSurface", "metadata"));
    }
    Ok(())
}

fn components_list(object: &Map<String, Value>) -> Result<&Vec<Value>, String> {
    let Some(components) = object.get("components") else {
        return Err("createSurface.components is required for interact surfaces".into());
    };
    let Some(list) = components.as_array() else {
        return Err("createSurface.components must be a non-empty array".into());
    };
    if list.is_empty() {
        return Err("createSurface.components must be a non-empty array".into());
    }
    if list.len() > super::MAX_COMPONENT_EXPANSION {
        return Err(format!(
            "createSurface.components exceeds the component limit of {}",
            super::MAX_COMPONENT_EXPANSION
        ));
    }
    Ok(list)
}

/// Every component's id, in list order, for the duplicate/root/child checks.
fn component_ids(list: &[Value]) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(list.len());
    for (index, component) in list.iter().enumerate() {
        let id = validate_component(component, &format!("createSurface.components[{index}]"))?;
        ids.push(id.to_string());
    }
    Ok(ids)
}

fn reject_duplicate_ids(ids: &[String]) -> Result<(), String> {
    for (index, id) in ids.iter().enumerate() {
        if ids[..index].contains(id) {
            return Err(format!(
                "createSurface.components[{index}]: duplicate component id '{id}'"
            ));
        }
    }
    Ok(())
}

fn require_root(ids: &[String]) -> Result<(), String> {
    if known_id(ids, "root") {
        return Ok(());
    }
    Err(
        "createSurface.components must include a component with id 'root' (the implicit \
         Surface container's child)"
            .into(),
    )
}

/// Button/Card `child` and Column `children` must name components the
/// renderer has: dangling ids would render nothing and a modal the user
/// cannot answer.
fn resolve_child_references(list: &[Value], ids: &[String]) -> Result<(), String> {
    for component in list {
        resolve_component_children(component, ids)?;
    }
    Ok(())
}

fn resolve_component_children(component: &Value, ids: &[String]) -> Result<(), String> {
    let id = component["id"].as_str().unwrap_or_default();
    let kind = component["component"].as_str().unwrap_or_default();
    let path_prefix = format!("createSurface.components (id '{id}')");
    match kind {
        "Button" | "Card" => resolve_single_child(component, kind, &path_prefix, ids),
        "Column" => resolve_column_children(component, &path_prefix, ids),
        _ => Ok(()),
    }
}

fn resolve_single_child(
    component: &Value,
    kind: &str,
    path_prefix: &str,
    ids: &[String],
) -> Result<(), String> {
    let child = component["child"].as_str().unwrap_or_default();
    if !known_id(ids, child) {
        return Err(format!(
            "{path_prefix}: {kind} child '{child}' does not match any component id"
        ));
    }
    Ok(())
}

fn resolve_column_children(
    component: &Value,
    path_prefix: &str,
    ids: &[String],
) -> Result<(), String> {
    if let Some(children) = component["children"].as_array() {
        return resolve_child_ids(children, path_prefix, ids);
    }
    if let Some(template) = component["children"].as_object() {
        return resolve_template_id(template, path_prefix, ids);
    }
    Ok(())
}

fn resolve_child_ids(children: &[Value], path_prefix: &str, ids: &[String]) -> Result<(), String> {
    for child in children {
        let child = child.as_str().unwrap_or_default();
        if !known_id(ids, child) {
            return Err(format!(
                "{path_prefix}: Column child '{child}' does not match any component id"
            ));
        }
    }
    Ok(())
}

fn resolve_template_id(
    template: &Map<String, Value>,
    path_prefix: &str,
    ids: &[String],
) -> Result<(), String> {
    let component_id = template["componentId"].as_str().unwrap_or_default();
    if !known_id(ids, component_id) {
        return Err(format!(
            "{path_prefix}: Column template componentId '{component_id}' does not match any \
             component id"
        ));
    }
    Ok(())
}

fn require_answerable(list: &[Value]) -> Result<(), String> {
    let answerable = list.iter().any(|component| {
        component["component"] == "Button"
            && component["action"]["event"]["name"]
                .as_str()
                .is_some_and(|name| !name.is_empty())
    });
    if answerable {
        return Ok(());
    }
    Err(
        "surface must define at least one Button with an action so the user can answer \
         (action.event.name)"
            .into(),
    )
}

fn known_id(ids: &[String], id: &str) -> bool {
    ids.iter().any(|known| known == id)
}
