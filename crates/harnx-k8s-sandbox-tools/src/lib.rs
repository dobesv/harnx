mod kubernetes;
pub mod leader_election;
#[cfg(test)]
#[path = "leader_election_tests.rs"]
mod leader_election_tests;
mod lifecycle;
mod mcp;
mod policy;
mod toolsets;

/// Install the process-default rustls [`CryptoProvider`] before any TLS client is built.
///
/// This binary's dependency graph links two rustls crypto providers — `ring` (via async-nats)
/// and `aws-lc-rs` (via the AWS SDK's hyper-rustls stack that `kube` uses) — and nothing else
/// installs a default. `kube::Client::try_default` builds its TLS config through
/// `rustls::ClientConfig::builder()` (kube's own `aws-lc-rs` fallback is compiled out because the
/// workspace doesn't enable `kube/aws-lc-rs`), which resolves the process-default provider and
/// panics with "Could not automatically determine the process-level CryptoProvider" when the
/// choice is ambiguous. Pin `ring` explicitly, matching the NATS TLS path in `harnx-nats-common`
/// (`crates/harnx-nats-common/src/connect.rs`). (`reqwest` is not affected: it resolves a provider
/// with `get_default()` plus an explicit `aws-lc-rs` fallback, never the bare builder.)
///
/// Idempotent: a provider installed by an earlier call is left in place, so calling this more
/// than once is safe.
///
/// Fixes <https://github.com/dobesv/harnx/issues/2103>.
///
/// [`CryptoProvider`]: async_nats::rustls::crypto::CryptoProvider
pub fn install_default_crypto_provider() {
    // Err means a provider was already installed; that's the idempotent case, so ignore it.
    let _ = async_nats::rustls::crypto::ring::default_provider().install_default();
}

pub use kubernetes::KubernetesSandboxApi;
pub use lifecycle::{
    SandboxApi, SandboxCondition, SandboxManager, SandboxManagerConfig, SandboxRecord,
    SandboxStatus,
};
pub use mcp::{
    McpCallError, McpCallErrorKind, McpCaller, McpCallerConfig, StreamableHttpMcpCaller,
};
pub use policy::{EndReason, FailureKind, TerminalClass, TerminalError};
pub use toolsets::{sandbox_toolsets, SandboxBinding, SandboxPorts, SANDBOX_CONTEXT_KEY};
