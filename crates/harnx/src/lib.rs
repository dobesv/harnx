pub mod agent_event_sink;
pub mod cli;
pub mod cli_event_sink;
pub mod serve;

pub mod test_utils;

pub use harnx_mcp as mcp;
pub use harnx_mcp::safety as mcp_safety;
pub use harnx_tui as tui;

// Re-export the runtime bundle so `crate::config::X`, `crate::client::X`,
// `crate::commands::X`, and `crate::tool::X` in harnx's remaining front-end
// code (acp/, main.rs, agent_event_sink.rs, cli_event_sink.rs, test_utils/)
// continue to resolve. After plan P46's extraction, these modules live in
// harnx-runtime.
pub use harnx_runtime::{client, commands, config, tool};
