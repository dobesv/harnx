//! `harnx-mcp` — MCP (Model Context Protocol) client support for the harnx
//! workspace. Provides the in-process [`McpManager`] that the harnx runtime
//! uses to talk to external MCP servers.

pub mod client;
pub mod config;
pub mod convert;

pub use client::McpManager;
pub use config::McpServerConfig;
