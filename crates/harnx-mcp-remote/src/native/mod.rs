//! Native NATS mode: per-user upstream MCP clients, connect pages and
//! per-user tool registrations (user-credentials plan, decisions 1, 7, 10).
//!
//! The mode is selected by `--credentials-key-file`. Everything in this module
//! runs behind that flag; the stdio proxy in `server`/`transport` is untouched
//! (R10). Secrets never cross a tool argument, result, session property or
//! log line: they live in the encrypted `harnx-credentials` stores and reach
//! only the per-user upstream client built here.

mod authenticated_http;
pub mod client_handler;
mod config;
pub mod connect_http;
pub mod oauth;
mod oauth_http;
pub mod pool;
pub mod registrations;
mod secret_redaction;
mod startup;
mod tls;
pub mod toolset;
mod upstream_auth;
mod upstream_connector;

pub use config::{parse_credential_ttl, AuthMethods, NativeConfig};
pub use startup::{run_native, NativeServer, NativeServerRuntime};

/// Refresh interval for per-user registrations (decision 5).
pub const DEFAULT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
