//! Library surface of `harnx-mcp-remote`.
//!
//! The binary keeps two modes with identical behavior to before, plus the new
//! native NATS mode: `native` hosts a [`harnx_toolset::Toolset`] that keeps
//! per-user upstream MCP clients and per-user tool registrations, while
//! `server` remains the unchanged stdio→Streamable-HTTP proxy (R10).

pub mod cli;
pub mod client_handler;
pub mod native;
pub mod server;
pub mod transport;
