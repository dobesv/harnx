#[macro_use]
extern crate log;

pub mod acp;
pub mod cli;
pub mod client;
pub mod config;
pub mod hooks;
pub mod mcp;
pub mod mcp_safety;
pub mod rag;
pub mod render;
#[allow(dead_code, unused_imports)]
pub mod repl;
pub mod serve;
pub mod tool;
pub mod tui;
pub mod ui_output;
#[macro_use]
pub mod utils;

#[cfg(test)]
pub mod test_utils;
