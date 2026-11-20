//! Inspecting a validated `a2ui` surface: the bound inputs and the declared
//! button actions a response can name.

use serde_json::{Map, Value};

/// One answerable input component and where its value lands in `data`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputField {
    /// The component's `id`.
    pub component_id: String,
    /// The data-model JSON pointer the value is bound to, e.g. `/choice`.
    pub path: String,
    /// The key in `InteractionResponse.data`: the path minus the leading `/`.
    pub key: String,
    /// Which input component kind this is.
    pub kind: InputKind,
    /// The label resolved against the surface's declared data model.
    pub label: Option<String>,
    /// Declared options (ChoicePicker only), in declaration order.
    pub options: Vec<OptionItem>,
    /// ChoicePicker: at most one selected option (variant absent or
    /// `mutuallyExclusive`, the Basic Catalog default).
    pub exclusive: bool,
}

/// The two input components of the supported subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    TextField,
    ChoicePicker,
}

/// One ChoicePicker option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionItem {
    /// The resolved option label, falling back to the option's `value` when
    /// its binding is missing or does not resolve to a string.
    pub label: String,
    /// The option's `value`, the data-model string a response must carry.
    pub value: String,
}

/// One answerable Button action.
#[derive(Clone, Debug)]
pub struct ButtonAction {
    /// The Button component's `id`.
    pub component_id: String,
    /// `action.event.name`, what a response's `action` names.
    pub name: String,
    /// The resolved text of the Button's `child` Text component, when it is one.
    pub label: Option<String>,
    /// The declared `action.event.context`, bindings unresolved.
    pub context: Map<String, Value>,
}

/// All input components of the surface, in components-list order.
pub fn input_fields(payload: &Value) -> Vec<InputField> {
    let components = components(payload);
    components
        .iter()
        .filter_map(|component| input_field(payload, component))
        .collect()
}

/// All Button actions of the surface, in components-list order.
pub fn button_actions(payload: &Value) -> Vec<ButtonAction> {
    buttons(payload)
        .iter()
        .map(|component| button_action(payload, component))
        .collect()
}

/// Whether `action` names a Button `action.event.name` declared in the
/// surface. `deny` (dismissal) is intentionally not special here.
pub fn action_is_declared(payload: &Value, action: &str) -> bool {
    buttons(payload)
        .iter()
        .any(|component| component["action"]["event"]["name"].as_str() == Some(action))
}

/// The first Button action whose `event.name` equals `action`, with its
/// declared context, or `None` when the surface declares no such action.
pub fn find_action(payload: &Value, action: &str) -> Option<ButtonAction> {
    buttons(payload)
        .iter()
        .map(|component| button_action(payload, component))
        .find(|button| button.name == action)
}

/// The Button components, or nothing for an unparseable payload, so every
/// inspector degrades to "nothing declared" rather than panicking.
fn buttons(payload: &Value) -> Vec<&Value> {
    components(payload)
        .iter()
        .filter(|component| component["component"] == "Button")
        .collect()
}

/// The components list of the requested surface.
pub fn components(payload: &Value) -> &[Value] {
    payload
        .get("createSurface")
        .and_then(|create| create.get("components"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn input_field(payload: &Value, component: &Value) -> Option<InputField> {
    let kind = match component.get("component")?.as_str()? {
        "TextField" => InputKind::TextField,
        "ChoicePicker" => InputKind::ChoicePicker,
        _ => return None,
    };
    let path = component
        .get("value")
        .and_then(|value| value.get("@path"))
        .and_then(Value::as_str)?
        .to_string();
    Some(InputField {
        component_id: component.get("id").and_then(Value::as_str)?.to_string(),
        key: data_key(&path).to_string(),
        label: super::resolve_dynamic_string(payload, component.get("label")),
        options: if kind == InputKind::ChoicePicker {
            option_items(payload, component)
        } else {
            Vec::new()
        },
        // Absent variant is the Basic Catalog default, mutuallyExclusive.
        exclusive: component.get("variant").and_then(Value::as_str) != Some("multipleSelection"),
        kind,
        path,
    })
}

fn button_action(payload: &Value, component: &Value) -> ButtonAction {
    ButtonAction {
        component_id: component
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        label: button_label(payload, component),
        context: component
            .get("action")
            .and_then(|action| action.get("event"))
            .and_then(|event| event.get("context"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        name: component
            .get("action")
            .and_then(|action| action.get("event"))
            .and_then(|event| event.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// The Button's visible text, resolved from its child Text component.
fn button_label(payload: &Value, component: &Value) -> Option<String> {
    let child = component.get("child").and_then(Value::as_str)?;
    let text = components(payload)
        .iter()
        .find(|sibling| sibling.get("id").and_then(Value::as_str) == Some(child))
        .and_then(|sibling| sibling.get("text"));
    super::resolve_dynamic_string(payload, text)
}

/// The declared options of a ChoicePicker, labels falling back to values.
fn option_items(payload: &Value, component: &Value) -> Vec<OptionItem> {
    component
        .get("options")
        .and_then(Value::as_array)
        .map_or(Vec::new(), |options| {
            options
                .iter()
                .filter_map(|option| {
                    let value = option.get("value").and_then(Value::as_str)?;
                    Some(OptionItem {
                        label: super::resolve_dynamic_string(payload, option.get("label"))
                            .unwrap_or_else(|| value.to_string()),
                        value: value.to_string(),
                    })
                })
                .collect()
        })
}

/// The `data` key for a data-model pointer: the pointer minus the leading `/`.
pub(super) fn data_key(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}
