//! Field and control models for interactive A2UI forms.

use super::tree::pointer_lookup;
use harnx_core::interaction::response::{InputField, OptionItem};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormTextField {
    pub id: String,
    pub key: String,
    pub path: String,
    pub label: String,
    pub value: String,
    pub cursor: usize,
    pub placeholder: Option<String>,
    pub variant: Option<String>,
}

impl FormTextField {
    pub fn from_field(field: InputField, comp: Option<&Value>, model: Option<&Value>) -> Self {
        let placeholder = comp
            .and_then(|c| c.get("placeholder"))
            .and_then(Value::as_str)
            .map(String::from);
        let variant = comp
            .and_then(|c| c.get("variant"))
            .and_then(Value::as_str)
            .map(String::from);
        let value = pointer_lookup(model, &field.path)
            .and_then(|v| {
                if let Some(s) = v.as_str() {
                    Some(s.to_string())
                } else if v.is_number() {
                    Some(v.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let cursor = value.len();
        let label = field.label.unwrap_or_else(|| field.key.clone());
        Self {
            id: field.component_id,
            key: field.key,
            path: field.path,
            label,
            value,
            cursor,
            placeholder,
            variant,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        if self.variant.as_deref() == Some("number") && !c.is_ascii_digit() && c != '.' && c != '-'
        {
            return;
        }
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_text_slice(&mut self, s: &str) {
        let clean = s.replace(['\r', '\n'], " ");
        let disallowed = self.variant.as_deref() == Some("number") && !is_valid_number_str(&clean);
        if disallowed {
            return;
        }
        self.value.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.value[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.value.remove(prev);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.value.len() {
            self.value.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.value[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.value.len() {
            if let Some(next) = self.value[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
            {
                self.cursor = next;
            } else {
                self.cursor = self.value.len();
            }
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.value.len();
    }
}

fn is_valid_number_str(s: &str) -> bool {
    !s.chars()
        .any(|c| !c.is_ascii_digit() && c != '.' && c != '-')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormChoicePicker {
    pub id: String,
    pub key: String,
    pub path: String,
    pub label: String,
    pub options: Vec<OptionItem>,
    pub exclusive: bool,
    pub selected: Vec<String>,
    pub highlighted: usize,
}

impl FormChoicePicker {
    pub fn from_field(field: InputField, model: Option<&Value>) -> Self {
        let selected = pointer_lookup(model, &field.path)
            .map(|v| match v {
                Value::Array(arr) => arr
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
                Value::String(s) => vec![s.clone()],
                _ => Vec::new(),
            })
            .unwrap_or_default();
        let label = field.label.unwrap_or_else(|| field.key.clone());
        Self {
            id: field.component_id,
            key: field.key,
            path: field.path,
            label,
            options: field.options,
            exclusive: field.exclusive,
            selected,
            highlighted: 0,
        }
    }

    pub fn move_up(&mut self) {
        self.highlighted = self.highlighted.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.options.is_empty() && self.highlighted + 1 < self.options.len() {
            self.highlighted += 1;
        }
    }

    pub fn toggle_highlighted(&mut self) {
        let Some(opt) = self.options.get(self.highlighted) else {
            return;
        };
        let val = &opt.value;
        if self.exclusive {
            self.selected = vec![val.clone()];
        } else if let Some(pos) = self.selected.iter().position(|v| v == val) {
            self.selected.remove(pos);
        } else {
            self.selected.push(val.clone());
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FormField {
    Text(FormTextField),
    Choice(FormChoicePicker),
}

impl FormField {
    pub fn id(&self) -> &str {
        match self {
            Self::Text(f) => &f.id,
            Self::Choice(f) => &f.id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormButton {
    pub component_id: String,
    pub name: String,
    pub label: String,
    pub variant: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FormItem {
    Text { text: String, caption: bool },
    Field(usize),
    Button(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusTarget {
    Field(usize),
    Button(usize),
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum A2UIAction {
    Submit(String),
}
