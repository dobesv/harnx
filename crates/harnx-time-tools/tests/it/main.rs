//! All of this crate's integration tests, built as one binary. A separate
//! binary per file links the whole dependency graph again for each, which
//! dominated build time; nextest still runs every test in its own process.

mod mcp_http_client;
mod missing_scope;
mod sigterm_shutdown;
