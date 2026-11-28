//! Library surface of `harnx-mcp-remote`.
//!
//! The binary runs either as a stdio→Streamable-HTTP proxy (`server`,
//! `transport`) or in native NATS mode (`native`, selected by
//! `--credentials-key-file`). Plain stdio proxy behavior is unchanged by
//! default; passing `--oauth-token-url` enables opt-in OAuth 2.0
//! `client_credentials` authentication via [`client_credentials`]. `native`
//! hosts a [`harnx_toolset::Toolset`] with per-user encrypted credential
//! stores, connect pages, and per-user upstream clients and tool
//! registrations, and shares HTTP auth, TLS, and redaction helpers with
//! client-credentials mode.

pub mod cli;
pub mod client_credentials;
pub mod client_handler;
pub mod native;
pub mod server;
pub mod transport;

pub(crate) use native::{authenticated_http, secret_redaction, upstream_auth::UpstreamAuth};
