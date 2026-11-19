//! Response-side contract for `a2ui` interactions: how an answered surface
//! becomes `InteractionResponse.data`, and what acceptance validates.
//!
//! Pinned source (a2ui-project/a2ui, commit `07735d6a654ae3ba25951a8753ddd57b344b1169`,
//! `specification/v1_0/json/renderer_to_agent.json` + `docs/a2ui_protocol.md`):
//! input components write the renderer's local data model synchronously, and
//! when the user triggers an action the renderer resolves the path bindings in
//! `action.event.context` against that data model into `action.context`.
//!
//! Harnx maps both onto the flat `data` object of `InteractionResponse`:
//!
//! - Every input component (TextField, ChoicePicker) contributes
//!   `data["<path minus leading '/'>"]` — for binding `{"@path": "/choice"}`,
//!   the key is `"choice"`. The value is the user's input at answer time.
//! - The chosen action's `event.context` entries are resolved per the pinned
//!   semantics (a `{"@path": ...}` binding resolves against the current data
//!   model — user input first, then the surface's declared `dataModel`, then
//!   `null`; literals pass through unchanged) and merged under their context
//!   keys, overriding input entries on collision.
//!
//! `deny` is dismissal, not an action of the surface: it carries no canonical
//! data, and acceptance ignores `data` on `deny`.
//!
//! Strictness at acceptance ([`validate_response`]): the action must name a
//! declared Button `action.event.name` or be `deny`; every `data` key must be
//! an input binding key or a declared context key of the chosen action;
//! input values are type-checked (TextField: string; ChoicePicker: array of
//! declared option values, at most one when the variant is exclusive).
//! Missing keys are allowed — a renderer may omit untouched inputs — and
//! context keys accept any JSON value, since a resolved binding carries
//! user-controlled data.

mod build;
mod inspect;
mod validate;

pub use build::{build_data, input_default, pointer_get, resolve_dynamic_string};
pub use inspect::{
    action_is_declared, button_actions, components, find_action, input_fields, ButtonAction,
    InputField, InputKind, OptionItem,
};
pub use validate::validate_response;

#[cfg(test)]
mod tests;
