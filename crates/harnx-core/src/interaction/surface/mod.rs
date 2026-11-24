//! Validation of `interact` surfaces against the pinned A2UI v1.0 Basic
//! Catalog subset (Text, Button, ChoicePicker, TextField, Card, Column).
//!
//! Pinned sources (a2ui-project/a2ui, commit `07735d6a654ae3ba25951a8753ddd57b344b1169`):
//! - `specification/v1_0/json/agent_to_renderer.json` — message envelope,
//!   `createSurface` shape, `ComponentsList`.
//! - `specification/v1_0/json/common_types.json` — `ComponentCommon`,
//!   `DynamicString`, `DynamicValue`, `DataBinding` (`{"@path": ...}`),
//!   `Action`, `Checkable`, `Extensions`.
//! - `catalogs/basic/v1/catalog.json` — the six supported component
//!   definitions and `Weight`.
//!
//! Harnx subset restrictions beyond the catalog (see plan note `t4-1-api`):
//! - Only the six components above; anything else is rejected.
//! - Renderer function calls (`{"@call": ...}`) are rejected; the runtime
//!   implements no Basic Catalog functions.
//! - Input component values (TextField, ChoicePicker) must be data-model
//!   bindings, so an answered action can carry the user's input back.
//! - The components list must contain `id: "root"` (the implicit Surface
//!   container's child) and at least one Button with an agent-side action,
//!   so the surface is renderable and answerable as a modal interaction.
//! - Component references must be acyclic, with at most 64 components along
//!   any path and 4096 visits after expanding shared references. The list
//!   itself is also capped at 4096 components, including unreachable ones.
//!
//! Split by validation stage: this file owns the message envelope, `create`
//! the createSurface structure, `component` per-component dispatch, `input`
//! and `layout` the component kinds, `action` Button actions, `binding` data
//! bindings and dynamic values, `util` shared JSON shape helpers.

mod action;
mod binding;
mod component;
mod create;
mod graph;
mod input;
mod layout;
#[cfg(test)]
mod tests;
mod util;

use serde_json::{Map, Value};
use util::expect_object;

pub(crate) use graph::{MAX_COMPONENT_DEPTH, MAX_COMPONENT_EXPANSION};

/// Catalog id of the pinned Basic Catalog v1.0, stamped into surfaces that
/// don't declare their own.
pub const BASIC_CATALOG_ID: &str =
    "https://a2ui.org/specification/v1_0/catalogs/basic/catalog.json";

/// The A2UI v1.0 Basic Catalog components harnx renders.
pub const SUPPORTED_COMPONENTS: &[&str] = &[
    "Text",
    "Button",
    "ChoicePicker",
    "TextField",
    "Card",
    "Column",
];

/// Validate a script-supplied `interact` surface message. Errors name the
/// offending path and problem; validation happens at request time.
pub fn validate_surface(message: &Value) -> Result<(), String> {
    let object = expect_object(message, "interact surface")?;
    expect_single_message_keys(object)?;
    util::expect_version(object)?;
    match object.get("createSurface") {
        Some(create) => create::validate_create_surface(create),
        None => Err("missing required field 'createSurface'".into()),
    }
}

/// The envelope takes exactly one message: `version` + `createSurface`.
/// `updateComponents` and friends belong to later turns a script cannot send.
fn expect_single_message_keys(object: &Map<String, Value>) -> Result<(), String> {
    for key in object.keys() {
        if key != "version" && key != "createSurface" {
            return Err(format!(
                "unexpected field '{key}': interact takes a single createSurface message"
            ));
        }
    }
    Ok(())
}
