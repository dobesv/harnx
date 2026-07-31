//! `harnx-mcp` — MCP (Model Context Protocol) support for the harnx
//! workspace. Provides the safety/validation utilities used by the
//! harnx-mcp-bash and harnx-fs-tools helper bins, plus the in-process MCP
//! client (`McpManager`) that the harnx runtime uses to talk to
//! external MCP servers.

pub mod client;
pub mod config;
pub mod content;
pub mod convert;
pub mod peer;
pub mod safety;
pub mod schema;

pub use client::McpManager;
pub use config::McpServerConfig;
