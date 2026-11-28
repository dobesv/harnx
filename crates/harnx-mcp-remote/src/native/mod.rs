//! Native NATS mode: per-user upstream MCP clients, connect pages, dynamic
//! client registration, and per-user tool registrations (user-credentials
//! plan, decisions 1, 7, 10).
//!
//! Native mode is selected by `--credentials-key-file` (and rejects
//! `--oauth-token-url`). Its runtime, connect pages, and per-user encrypted
//! `harnx-credentials` stores run only in native mode, leaving plain/default
//! stdio proxy behavior in `server`/`transport` unchanged. Crate-internal HTTP
//! auth, TLS, secret-reading, and redaction helpers live here and are shared
//! with opt-in client-credentials proxy mode.

pub(crate) mod authenticated_http;
pub mod client_handler;
mod config;
pub mod connect_http;
pub mod oauth;
pub(crate) mod oauth_http;
pub mod pool;
pub mod registrations;
pub(crate) mod secret_redaction;
mod startup;
pub(crate) mod tls;
pub mod toolset;
pub(crate) mod upstream_auth;
mod upstream_connector;

pub use config::{
    parse_credential_ttl, AuthMethods, NativeConfig, OAUTH_TOKEN_URL_NATIVE_REJECTION,
};
pub use startup::{run_native, NativeServer, NativeServerRuntime};

/// Native-mode entry for the binary. Mixed native+client-credentials mode is
/// rejected before any logging: the native startup log below prints the raw
/// configured URL, which can carry embedded credentials, and the conflict is
/// knowable without touching the URL. `NativeConfig::from_cli` keeps the same
/// static gate for direct callers.
pub async fn run_native_mode(cli: crate::cli::Cli) -> anyhow::Result<()> {
    if cli.oauth_token_url.is_some() {
        anyhow::bail!(OAUTH_TOKEN_URL_NATIVE_REJECTION);
    }
    let _ = harnx_core::logging::init(harnx_core::logging::LogSink::Stderr);
    log::info!(
        "harnx-mcp-remote v{}: starting native mode for {}",
        env!("CARGO_PKG_VERSION"),
        cli.url
    );
    let config = NativeConfig::from_cli(&cli)?;
    run_native(config).await
}

/// Refresh interval for per-user registrations (decision 5).
pub const DEFAULT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
