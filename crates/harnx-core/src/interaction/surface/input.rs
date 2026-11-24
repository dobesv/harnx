//! Input components: ChoicePicker and TextField, whose values must bind to
//! data-model paths so an accepted action carries the user's input back.

use serde_json::{Map, Value};

use super::binding::{
    optional_dynamic_field, required_dynamic_field, required_input_binding, validate_enum,
};
use super::util::{expect_object, property_path, reject_unknown_keys};

const CHOICE_VARIANTS: &[&str] = &["mutuallyExclusive", "multipleSelection"];
const CHOICE_DISPLAY_STYLES: &[&str] = &["checkbox", "chips"];
const TEXT_FIELD_VARIANTS: &[&str] = &["shortText", "longText", "number", "obscured"];

pub(super) fn validate_choice_picker(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    optional_dynamic_field(object, "label", component_path)?;
    validate_enum(
        object.get("variant"),
        CHOICE_VARIANTS,
        &property_path(component_path, "variant"),
    )?;
    validate_enum(
        object.get("displayStyle"),
        CHOICE_DISPLAY_STYLES,
        &property_path(component_path, "displayStyle"),
    )?;
    validate_filterable(object, component_path)?;
    validate_options(object, component_path)?;
    required_input_binding(object, "value", component_path)
}

fn validate_filterable(object: &Map<String, Value>, component_path: &str) -> Result<(), String> {
    if let Some(filterable) = object.get("filterable") {
        return ensure_boolean(filterable, component_path);
    }
    Ok(())
}

fn ensure_boolean(value: &Value, component_path: &str) -> Result<(), String> {
    if !value.is_boolean() {
        return Err(format!("{}.filterable must be a boolean", component_path));
    }
    Ok(())
}

fn validate_options(object: &Map<String, Value>, component_path: &str) -> Result<(), String> {
    let options = required_options(object, component_path)?;
    validate_option_list(options, component_path)
}

/// Options may be absent, but a picker without options is unanswerable, so
/// a present value must be a non-empty array.
fn required_options<'a>(
    object: &'a Map<String, Value>,
    component_path: &str,
) -> Result<&'a [Value], String> {
    let Some(list) = object.get("options").and_then(Value::as_array) else {
        return Err(format!(
            "{component_path}: missing required field 'options' (a non-empty array)"
        ));
    };
    if list.is_empty() {
        return Err(format!(
            "{}.options must be a non-empty array",
            component_path
        ));
    }
    Ok(list)
}

fn validate_option_list(options: &[Value], component_path: &str) -> Result<(), String> {
    for (index, option) in options.iter().enumerate() {
        validate_option(option, &format!("{component_path}.options[{index}]"))?;
    }
    Ok(())
}

fn validate_option(option: &Value, option_path: &str) -> Result<(), String> {
    let option = expect_object(option, &format!("{option_path}: option"))?;
    reject_unknown_keys(option, &["label", "value"], option_path, "field")?;
    validate_option_label(option, option_path)?;
    validate_option_value(option, option_path)
}

fn validate_option_label(option: &Map<String, Value>, option_path: &str) -> Result<(), String> {
    if let Some(label) = option.get("label") {
        return super::binding::validate_dynamic_string(
            label,
            &property_path(option_path, "label"),
        );
    }
    Err(format!("{option_path}: missing required field 'label'"))
}

fn validate_option_value(option: &Map<String, Value>, option_path: &str) -> Result<(), String> {
    match option.get("value") {
        Some(Value::String(value)) if !value.is_empty() => Ok(()),
        Some(_) => Err(format!("{}.value must be a non-empty string", option_path)),
        None => Err(format!("{option_path}: missing required field 'value'")),
    }
}

pub(super) fn validate_text_field(
    object: &Map<String, Value>,
    component_path: &str,
) -> Result<(), String> {
    required_dynamic_field(object, "label", component_path)?;
    optional_dynamic_field(object, "placeholder", component_path)?;
    validate_enum(
        object.get("variant"),
        TEXT_FIELD_VARIANTS,
        &property_path(component_path, "variant"),
    )?;
    required_input_binding(object, "value", component_path)
}
