//! MCP server exposing harnx tools and agents-as-tools.
//!
//! This crate implements `harnx-mcp-server`, a binary that exposes harnx tools
//! (fs, bash, plans, attachments, etc.) and harnx agents-as-tools over MCP
//! (Model Context Protocol) via stdio or Streamable HTTP transports.
//!
//! # Architecture
//!
//! Each MCP connection is backed by a dedicated harnx session. On connection,
//! the server creates a tool reservation with a worker, which starts the
//! selected tool servers for the connection's lifetime. When the connection
//! closes, the reservation is released and tool servers shut down.
//!
//! # Transports
//!
//! - **stdio**: Single connection per process. Suitable for local tool usage.
//! - **HTTP**: Multiple concurrent connections via Streamable HTTP at `/mcp`.
//!   Each MCP session gets its own backing harnx session.
//!
//! # Tool Selection
//!
//! Tools are selected via `--use-tools` (repeatable, comma-separated) or
//! `HARNX_MCP_USE_TOOLS` environment variable. Syntax matches agent `use_tools`:
//! brace expansion, globs, and toolset aliases. Empty selector list is an error;
//! non-matching selectors are valid (empty catalog).
//!
//! # Package Context
//!
//! The `--package` flag controls tool naming: tools belonging to this package
//! appear with package-unqualified names (e.g. `fs_read`, stripping the package
//! prefix, not the server prefix); cross-package tools use `pkg__server_tool`
//! qualification, matching what an agent in that package would see.
//!
//! # Cluster Selection
//!
//! `--cluster <name>` targets a shared NATS cluster. When omitted, `HARNX_NATS_SERVER`
//! env selects the cluster. If neither is set, uses the local `__local__` cluster
//! (embedded broker + child worker).
//!
//! # Limitations
//!
//! - No HITL/confirmation hooks. Do not expose tools that require confirmation.
//! - Direct tool calls do not run agent tool-round hooks.
//! - No MCP resources or prompts.
//!
//! See `docs/transports.md` for transport ownership and inactivity defaults.

pub mod bootstrap;
pub mod connection;
pub mod handler;
pub mod transport;

/// Environment variable for tool selectors.
pub const HARNX_MCP_USE_TOOLS_ENV: &str = "HARNX_MCP_USE_TOOLS";
/// Environment variable for package context.
pub const HARNX_MCP_PACKAGE_ENV: &str = "HARNX_MCP_PACKAGE";
/// Default HTTP port for harnx-mcp-server (distinct from toolset defaults 3000-3007).
pub const DEFAULT_MCP_HTTP_PORT: u16 = 3010;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_cli_defaults() {
        assert_eq!(HARNX_MCP_USE_TOOLS_ENV, "HARNX_MCP_USE_TOOLS");
        assert_eq!(HARNX_MCP_PACKAGE_ENV, "HARNX_MCP_PACKAGE");
        assert_eq!(DEFAULT_MCP_HTTP_PORT, 3010);
    }
}
