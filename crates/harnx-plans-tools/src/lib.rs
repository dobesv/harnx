//! NATS-backed plan/task/note tools using canonical `cid:plan:` URLs.

mod script_schema;
mod server;
mod tool_templates;
mod toolset;

pub use toolset::PlansToolset;
