//! Surface component tree walking and data model pointer resolution.

use super::fields::{FormButton, FormField, FormItem};
pub(super) use harnx_core::interaction::response::pointer_get as pointer_lookup;
use serde_json::Value;
use std::collections::HashSet;

pub(super) struct WalkContext<'a> {
    pub comps: &'a [Value],
    pub button_child_ids: &'a HashSet<String>,
    pub fields: &'a [FormField],
    pub buttons: &'a [FormButton],
}

// qual:allow(iosp) — tracks visited component ids and dispatches containers versus leaves
pub(super) fn walk_component(
    ctx: &WalkContext<'_>,
    id: &str,
    items: &mut Vec<FormItem>,
    visited: &mut HashSet<String>,
) {
    if !visited.insert(id.to_string()) {
        return;
    }
    let Some(comp) = ctx
        .comps
        .iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(id))
    else {
        return;
    };
    match comp.get("component").and_then(Value::as_str) {
        Some("Card" | "Column") => walk_container(ctx, comp, items, visited),
        Some(_) => walk_leaf(ctx, comp, id, items),
        None => {}
    }
}

// qual:allow(iosp) — traverses Card child or Column children recursively
fn walk_container(
    ctx: &WalkContext<'_>,
    comp: &Value,
    items: &mut Vec<FormItem>,
    visited: &mut HashSet<String>,
) {
    if let Some(child) = comp.get("child").and_then(Value::as_str) {
        walk_component(ctx, child, items, visited);
    } else if let Some(children) = comp.get("children").and_then(Value::as_array) {
        for child_id in children.iter().filter_map(Value::as_str) {
            if !ctx.button_child_ids.contains(child_id) {
                walk_component(ctx, child_id, items, visited);
            }
        }
    }
}

fn walk_leaf(ctx: &WalkContext<'_>, comp: &Value, id: &str, items: &mut Vec<FormItem>) {
    let kind = comp
        .get("component")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "Text" if !ctx.button_child_ids.contains(id) => {
            if let Some(text) = comp.get("text").and_then(Value::as_str) {
                let caption = comp.get("variant").and_then(Value::as_str) == Some("caption");
                items.push(FormItem::Text {
                    text: text.to_string(),
                    caption,
                });
            }
        }
        "TextField" | "ChoicePicker" => {
            if let Some(idx) = ctx.fields.iter().position(|f| f.id() == id) {
                items.push(FormItem::Field(idx));
            }
        }
        "Button" => {
            if let Some(idx) = ctx.buttons.iter().position(|b| b.component_id == id) {
                items.push(FormItem::Button(idx));
            }
        }
        _ => {}
    }
}
