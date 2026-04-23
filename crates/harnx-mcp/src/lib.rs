//! `harnx-mcp` — MCP (Model Context Protocol) support for the harnx
//! workspace. Provides the safety/validation utilities used by the
//! harnx-mcp-bash and harnx-mcp-fs helper bins, plus the in-process MCP
//! client (`McpManager`) that the harnx runtime uses to talk to
//! external MCP servers.

pub mod client;
pub mod config;
pub mod convert;
pub mod safety;

pub use client::McpManager;
pub use config::McpServerConfig;
