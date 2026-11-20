//! Plain-text rendering of an `a2ui` surface for non-graphical frontends.

use std::collections::HashSet;

use serde_json::Value;

use crate::interaction::response::{
    button_actions, components, input_default, resolve_dynamic_string,
};
use crate::interaction::surface::{MAX_COMPONENT_DEPTH, MAX_COMPONENT_EXPANSION};

/// Render the surface as plain text: its text lines, its inputs with their
/// declared options and defaults (obscured values redacted), then its actions.
/// Stored payloads need not have passed the current graph validator.
pub fn render(payload: &Value) -> String {
    let mut renderer = Renderer::default();
    renderer.walk_root(payload);
    let mut text = renderer.lines.join("\n");
    let actions = actions_line(payload);
    if !actions.is_empty() {
        text.push_str("\n\n");
        text.push_str(&actions);
        text.push_str(
            "\nAnswer with an action (name or label), optionally followed by key=value pairs, \
             or `deny`.",
        );
    }
    text
}

#[derive(Default)]
struct Renderer {
    lines: Vec<String>,
    active: HashSet<String>,
    visits: usize,
    truncated: bool,
}

impl Renderer {
    /// Render in layout order, starting at the `root` component.
    fn walk_root(&mut self, payload: &Value) {
        if let Some(root) = components(payload)
            .iter()
            .find(|component| component.get("id").and_then(Value::as_str) == Some("root"))
        {
            self.walk(payload, root, 1);
        }
    }

    fn walk(&mut self, payload: &Value, component: &Value, depth: usize) {
        let id = component
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if depth > MAX_COMPONENT_DEPTH
            || self.visits >= MAX_COMPONENT_EXPANSION
            || !self.active.insert(id.to_string())
        {
            self.report_truncation();
            return;
        }
        self.visits += 1;
        match component.get("component").and_then(Value::as_str) {
            Some("Text") => self.text_line(payload, component),
            Some("TextField") => self.text_field_line(payload, component),
            Some("ChoicePicker") => self.choice_picker_lines(payload, component),
            Some("Card") => self.walk_child(payload, component, depth),
            Some("Column") => self.walk_children(payload, component, depth),
            // Buttons render in the actions footer, not inline.
            _ => {}
        }
        // Track the active path, not every visited id: DAGs may share children.
        self.active.remove(id);
    }

    fn report_truncation(&mut self) {
        if !self.truncated {
            self.lines
                .push("[surface truncated: cyclic or oversized component graph]".into());
            self.truncated = true;
        }
    }

    fn text_line(&mut self, payload: &Value, component: &Value) {
        if let Some(text) = resolve_dynamic_string(payload, component.get("text")) {
            self.lines.push(text);
        }
    }

    fn text_field_line(&mut self, payload: &Value, component: &Value) {
        let Some(path) = component
            .get("value")
            .and_then(|value| value.get("@path"))
            .and_then(Value::as_str)
        else {
            return;
        };
        // Obscured is visual masking, not a secret-storage guarantee. Never
        // copy its default into a text transcript, even when it is empty.
        let default = if component.get("variant").and_then(Value::as_str) == Some("obscured") {
            "[redacted]".to_string()
        } else {
            input_default(payload, path).unwrap_or_default()
        };
        let label = resolve_dynamic_string(payload, component.get("label"))
            .unwrap_or_else(|| "(input)".into());
        self.lines.push(format!("{label}: {default}"));
    }

    fn choice_picker_lines(&mut self, payload: &Value, component: &Value) {
        if let Some(label) = resolve_dynamic_string(payload, component.get("label")) {
            self.lines.push(format!("{label}:"));
        }
        for (index, option) in component
            .get("options")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .enumerate()
        {
            let value = option
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let label = resolve_dynamic_string(payload, option.get("label"))
                .unwrap_or_else(|| value.to_string());
            self.lines
                .push(format!("  {}. {} [{}]", index + 1, label, value));
        }
    }

    fn walk_child(&mut self, payload: &Value, component: &Value, depth: usize) {
        if let Some(child) = component.get("child").and_then(Value::as_str) {
            if let Some(component) = components(payload)
                .iter()
                .find(|sibling| sibling.get("id").and_then(Value::as_str) == Some(child))
            {
                self.walk(payload, component, depth + 1);
            }
        }
    }

    /// Template children are dynamic and are not expanded in the fallback.
    fn walk_children(&mut self, payload: &Value, component: &Value, depth: usize) {
        for child in component
            .get("children")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
        {
            if self.visits >= MAX_COMPONENT_EXPANSION {
                self.report_truncation();
                break;
            }
            if let Some(child) = child.as_str() {
                if let Some(component) = components(payload)
                    .iter()
                    .find(|sibling| sibling.get("id").and_then(Value::as_str) == Some(child))
                {
                    self.walk(payload, component, depth + 1);
                }
            }
        }
    }
}

/// The actions footer: every declared Button as `name (label)`, plus the
/// always-available `deny` dismissal.
fn actions_line(payload: &Value) -> String {
    let actions = button_actions(payload);
    if actions.is_empty() {
        return String::new();
    }
    let mut names = actions
        .iter()
        .map(|action| match &action.label {
            Some(label) => format!("{} ({})", action.name, label),
            None => action.name.clone(),
        })
        .collect::<Vec<_>>();
    names.push("deny".to_string());
    format!("Actions: {}", names.join(", "))
}
