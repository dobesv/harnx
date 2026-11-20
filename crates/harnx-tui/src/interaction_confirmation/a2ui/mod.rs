//! In-memory state and key handling for interactive A2UI modal forms.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harnx_core::interaction::response::{
    button_actions, components, input_fields, ButtonAction, InputField, InputKind,
};
use harnx_core::interaction::surface::validate_surface;
use serde_json::{Map, Value};
use std::collections::HashSet;

mod fields;
mod tree;

pub(crate) use fields::{
    A2UIAction, FocusTarget, FormButton, FormChoicePicker, FormField, FormItem, FormTextField,
};
use tree::{walk_component, WalkContext};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct A2UIFormState {
    pub fields: Vec<FormField>,
    pub buttons: Vec<FormButton>,
    pub items: Vec<FormItem>,
    pub focus: FocusTarget,
}

impl A2UIFormState {
    // qual:allow(iosp) — validates surface and initializes form fields, buttons, and layout items
    pub fn new(payload: &Value) -> Option<Self> {
        if validate_surface(payload).is_err() {
            return None;
        }
        let raw_buttons = button_actions(payload);
        if raw_buttons.is_empty() {
            return None;
        }
        let comps = components(payload);
        let data_model = payload
            .get("createSurface")
            .and_then(|c| c.get("dataModel"));

        let fields = build_fields(input_fields(payload), comps, data_model);
        let buttons = build_buttons(raw_buttons, comps);
        let btn_children = button_child_ids(comps);
        let items = build_items(comps, &btn_children, &fields, &buttons);
        let focus = initial_focus(&fields, &buttons);

        Some(Self {
            fields,
            buttons,
            items,
            focus,
        })
    }

    pub fn values(&self) -> Map<String, Value> {
        let mut map = Map::new();
        for field in &self.fields {
            match field {
                FormField::Text(t) => {
                    map.insert(t.key.clone(), Value::String(t.value.clone()));
                }
                FormField::Choice(c) => {
                    map.insert(
                        c.key.clone(),
                        Value::Array(c.selected.iter().cloned().map(Value::String).collect()),
                    );
                }
            }
        }
        map
    }

    pub fn primary_action(&self) -> Option<String> {
        self.buttons
            .iter()
            .find(|b| b.variant.as_deref() == Some("primary"))
            .or_else(|| self.buttons.first())
            .map(|b| b.name.clone())
    }

    pub fn focus_next(&mut self) {
        let total = self.fields.len() + self.buttons.len() + 1;
        let next = (self.focus_index() + 1) % total;
        self.set_focus_by_index(next);
    }

    pub fn focus_prev(&mut self) {
        let total = self.fields.len() + self.buttons.len() + 1;
        let prev = (self.focus_index() + total - 1) % total;
        self.set_focus_by_index(prev);
    }

    fn focus_index(&self) -> usize {
        let f_len = self.fields.len();
        match self.focus {
            FocusTarget::Field(i) => i.min(f_len.saturating_sub(1)),
            FocusTarget::Button(i) => f_len + i.min(self.buttons.len().saturating_sub(1)),
            FocusTarget::Note => f_len + self.buttons.len(),
        }
    }

    fn set_focus_by_index(&mut self, idx: usize) {
        let f_len = self.fields.len();
        let b_len = self.buttons.len();
        if idx < f_len {
            self.focus = FocusTarget::Field(idx);
        } else if idx < f_len + b_len {
            self.focus = FocusTarget::Button(idx - f_len);
        } else {
            self.focus = FocusTarget::Note;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<A2UIAction> {
        match self.focus {
            FocusTarget::Field(idx) => {
                let primary = self.primary_action();
                let field = self.fields.get_mut(idx)?;
                let (action, nav) = handle_field_key(field, key, primary.as_deref());
                if let Some(nav) = nav {
                    match nav {
                        FocusNav::Next => self.focus_next(),
                        FocusNav::Prev => self.focus_prev(),
                    }
                }
                action
            }
            FocusTarget::Button(idx) => {
                let btn = self.buttons.get(idx)?;
                let (action, nav) = handle_button_key(btn, key);
                if let Some(nav) = nav {
                    match nav {
                        FocusNav::Next => self.focus_next(),
                        FocusNav::Prev => self.focus_prev(),
                    }
                }
                action
            }
            FocusTarget::Note => None,
        }
    }

    pub fn paste_text(&mut self, text: &str) -> bool {
        let FocusTarget::Field(idx) = self.focus else {
            return false;
        };
        match self.fields.get_mut(idx) {
            Some(FormField::Text(tf)) => {
                tf.insert_text_slice(text);
                true
            }
            _ => false,
        }
    }
}

enum FocusNav {
    Next,
    Prev,
}

fn handle_field_key(
    field: &mut FormField,
    key: KeyEvent,
    primary: Option<&str>,
) -> (Option<A2UIAction>, Option<FocusNav>) {
    match field {
        FormField::Text(tf) => handle_text_field_key(tf, key, primary),
        FormField::Choice(cp) => {
            handle_choice_key(cp, key);
            (None, None)
        }
    }
}

fn handle_text_field_key(
    tf: &mut FormTextField,
    key: KeyEvent,
    primary: Option<&str>,
) -> (Option<A2UIAction>, Option<FocusNav>) {
    match (key.code, key.modifiers) {
        (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => tf.insert_char(c),
        (KeyCode::Backspace, _) => tf.backspace(),
        (KeyCode::Delete, _) => tf.delete(),
        (KeyCode::Left, _) => tf.move_left(),
        (KeyCode::Right, _) => tf.move_right(),
        (KeyCode::Home, _) => tf.move_home(),
        (KeyCode::End, _) => tf.move_end(),
        (KeyCode::Down, KeyModifiers::NONE) => return (None, Some(FocusNav::Next)),
        (KeyCode::Up, KeyModifiers::NONE) => return (None, Some(FocusNav::Prev)),
        (KeyCode::Enter, KeyModifiers::NONE) => {
            if let Some(action) = primary {
                return (Some(A2UIAction::Submit(action.to_string())), None);
            }
        }
        _ => {}
    }
    (None, None)
}

fn handle_choice_key(cp: &mut FormChoicePicker, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Up, _) => cp.move_up(),
        (KeyCode::Down, _) => cp.move_down(),
        (KeyCode::Char(' ') | KeyCode::Enter, _) => cp.toggle_highlighted(),
        _ => {}
    }
}

fn handle_button_key(btn: &FormButton, key: KeyEvent) -> (Option<A2UIAction>, Option<FocusNav>) {
    match (key.code, key.modifiers) {
        (KeyCode::Enter | KeyCode::Char(' '), KeyModifiers::NONE) => {
            (Some(A2UIAction::Submit(btn.name.clone())), None)
        }
        (KeyCode::Left | KeyCode::Up, _) => (None, Some(FocusNav::Prev)),
        (KeyCode::Right | KeyCode::Down, _) => (None, Some(FocusNav::Next)),
        _ => (None, None),
    }
}

fn build_fields(
    raw: Vec<InputField>,
    comps: &[Value],
    data_model: Option<&Value>,
) -> Vec<FormField> {
    let mut fields = Vec::new();
    for field in raw {
        let comp = comps
            .iter()
            .find(|c| c.get("id").and_then(Value::as_str) == Some(&field.component_id));
        match field.kind {
            InputKind::TextField => {
                fields.push(FormField::Text(FormTextField::from_field(
                    field, comp, data_model,
                )));
            }
            InputKind::ChoicePicker => {
                fields.push(FormField::Choice(FormChoicePicker::from_field(
                    field, data_model,
                )));
            }
        }
    }
    fields
}

fn build_buttons(raw: Vec<ButtonAction>, comps: &[Value]) -> Vec<FormButton> {
    let mut buttons = Vec::new();
    for btn in raw {
        let comp = comps
            .iter()
            .find(|c| c.get("id").and_then(Value::as_str) == Some(&btn.component_id));
        let variant = comp
            .and_then(|c| c.get("variant"))
            .and_then(Value::as_str)
            .map(String::from);
        buttons.push(FormButton {
            component_id: btn.component_id,
            name: btn.name,
            label: btn.label.unwrap_or_else(|| "Submit".into()),
            variant,
        });
    }
    buttons
}

fn button_child_ids(comps: &[Value]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for comp in comps {
        if comp.get("component").and_then(Value::as_str) == Some("Button") {
            if let Some(child_id) = comp.get("child").and_then(Value::as_str) {
                ids.insert(child_id.to_string());
            }
        }
    }
    ids
}

// qual:allow(iosp) — walks component tree and appends unreferenced fields and buttons
fn build_items(
    comps: &[Value],
    btn_children: &HashSet<String>,
    fields: &[FormField],
    buttons: &[FormButton],
) -> Vec<FormItem> {
    let mut items = Vec::new();
    let mut visited = HashSet::new();
    let ctx = WalkContext {
        comps,
        button_child_ids: btn_children,
        fields,
        buttons,
    };
    walk_component(&ctx, "root", &mut items, &mut visited);

    for idx in 0..fields.len() {
        if !items
            .iter()
            .any(|item| matches!(item, FormItem::Field(i) if *i == idx))
        {
            items.push(FormItem::Field(idx));
        }
    }
    for idx in 0..buttons.len() {
        if !items
            .iter()
            .any(|item| matches!(item, FormItem::Button(i) if *i == idx))
        {
            items.push(FormItem::Button(idx));
        }
    }
    items
}

fn initial_focus(fields: &[FormField], buttons: &[FormButton]) -> FocusTarget {
    if !fields.is_empty() {
        FocusTarget::Field(0)
    } else if !buttons.is_empty() {
        FocusTarget::Button(0)
    } else {
        FocusTarget::Note
    }
}
