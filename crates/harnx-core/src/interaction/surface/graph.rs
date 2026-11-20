//! Bounds on component references, including shared subgraphs and templates.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// Component budget shared by every renderer. The web validator duplicates
/// these values as `MAX_A2UI_DEPTH` and `MAX_A2UI_EXPANSION` in
/// `web/src/a2ui/buildData.ts`; change both sides together so a surface that
/// passes validation also renders within the web budget.
/// Maximum number of components along a rendered path, including the root.
pub(crate) const MAX_COMPONENT_DEPTH: usize = 64;
/// Maximum component visits after expanding shared references in one tree.
pub(crate) const MAX_COMPONENT_EXPANSION: usize = 4096;

/// Validate every component, even those not currently reachable from `root`.
/// Memoized subtree sizes count shared children at every reference, so a small
/// DAG cannot ask a renderer to produce exponentially many components.
pub(super) fn validate_component_graph(list: &[Value]) -> Result<(), String> {
    let children: HashMap<_, _> = list
        .iter()
        .map(|component| {
            (
                component["id"].as_str().unwrap_or_default(),
                child_ids(component),
            )
        })
        .collect();
    let mut validator = GraphValidator {
        children: &children,
        active: HashSet::new(),
        sizes: HashMap::new(),
    };
    for component in list {
        validator.measure(component["id"].as_str().unwrap_or_default(), 1)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct GraphSize {
    depth: usize,
    expansion: usize,
}

struct GraphValidator<'a> {
    children: &'a HashMap<&'a str, Vec<&'a str>>,
    active: HashSet<&'a str>,
    sizes: HashMap<&'a str, GraphSize>,
}

impl<'a> GraphValidator<'a> {
    fn measure(&mut self, id: &'a str, depth: usize) -> Result<GraphSize, String> {
        // Bound the validator's own recursion before entering another frame.
        if depth > MAX_COMPONENT_DEPTH {
            return Err(depth_error());
        }
        if let Some(size) = self.sizes.get(id) {
            if depth - 1 + size.depth > MAX_COMPONENT_DEPTH {
                return Err(depth_error());
            }
            return Ok(*size);
        }
        if !self.active.insert(id) {
            return Err("createSurface.components contains a component reference cycle".into());
        }
        let mut size = GraphSize {
            depth: 1,
            expansion: 1,
        };
        let children = self.children;
        for child in &children[id] {
            let child_size = self.measure(child, depth + 1)?;
            size.depth = size.depth.max(child_size.depth + 1);
            size.expansion += child_size.expansion;
            if size.expansion > MAX_COMPONENT_EXPANSION {
                return Err(format!(
                    "createSurface.components exceeds the expansion limit of {MAX_COMPONENT_EXPANSION}"
                ));
            }
        }
        self.active.remove(id);
        self.sizes.insert(id, size);
        Ok(size)
    }
}

fn depth_error() -> String {
    format!("createSurface.components exceeds the depth limit of {MAX_COMPONENT_DEPTH}")
}

/// Child references in layout order. A template contributes its component
/// reference once; data-dependent template repetition belongs to the renderer.
fn child_ids(component: &Value) -> Vec<&str> {
    match component["component"].as_str() {
        Some("Card" | "Button") => component["child"].as_str().into_iter().collect(),
        Some("Column") => match component["children"].as_array() {
            Some(children) => children.iter().filter_map(Value::as_str).collect(),
            None => component["children"]["componentId"]
                .as_str()
                .into_iter()
                .collect(),
        },
        _ => Vec::new(),
    }
}
