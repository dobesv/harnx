//! Plain-text fallback for `a2ui` surfaces: a renderer for frontends without
//! A2UI widgets (A2A text replies, ACP, the CLI answer command), and a parser
//! that turns a simple text answer into the canonical response every
//! frontend's answer maps to.
//!
//! Grammar of an answer (one line, whitespace-separated):
//!
//! ```text
//! <action> [key=value ...]
//! ```
//!
//! - `<action>` names the action to trigger: a Button's `action.event.name`,
//!   its label, or `deny` for dismissal.
//! - Each `key=value` sets one input: `key` is the input's data key (or the
//!   full pointer, e.g. `/name=Ada`), the value a single token. A
//!   ChoicePicker value is a comma-separated list of option values or labels,
//!   e.g. `choice=staging` or `choice=Staging,Production`.
//!
//! Untouched inputs default through [`super::response::build_data`], so a
//! parsed answer and a graphical renderer's answer produce the same `data`.

mod parse;
mod render;

pub use parse::parse;
pub use render::render;

#[cfg(test)]
mod tests;
