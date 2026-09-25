//! Regression test for the startup crash in issue #2103.
//!
//! `harnx-k8s-sandbox-tools` links two rustls crypto providers — `ring` (via async-nats) and
//! `aws-lc-rs` (via the AWS SDK's hyper-rustls stack that `kube` uses) — and nothing installs a
//! process default. `kube::Client::try_default` and `reqwest::Client` both build their TLS config
//! through `rustls::ClientConfig::builder()`, which resolves the process-default provider and
//! panics with "Could not automatically determine the process-level CryptoProvider" when the
//! choice is ambiguous. `install_default_crypto_provider` pins `ring` up front so startup no
//! longer panics.
//!
//! This must live in a crate whose graph also pulls in the AWS SDK. A crate that resolves rustls
//! with `ring` alone cannot reproduce the ambiguity, so the assertion would pass no matter what
//! the code did. See `crates/harnx-runtime/tests/tls_client_config.rs` for the same reasoning.
//!
//! The test mutates the process-global default provider, so it needs nextest's per-test process
//! isolation and must not run under `cargo test`.

use async_nats::rustls::crypto::CryptoProvider;
use async_nats::rustls::{ClientConfig, RootCertStore};
use harnx_k8s_sandbox_tools::install_default_crypto_provider;

#[test]
fn install_default_crypto_provider_makes_client_config_builder_safe() {
    harnx_core::require_nextest();

    // Nothing in the dependency graph installs a provider automatically — that missing default is
    // the root of the crash. In this fresh, isolated process it's still unset, so a later
    // `ClientConfig::builder()` would have to resolve the ambiguous process default and panic.
    assert!(
        CryptoProvider::get_default().is_none(),
        "no crypto provider should be installed before install_default_crypto_provider runs; \
         if one is, the ambiguity this test guards against is no longer reproducible here",
    );

    install_default_crypto_provider();

    assert!(
        CryptoProvider::get_default().is_some(),
        "install_default_crypto_provider must install a process-default crypto provider",
    );

    // The exact call kube and reqwest make internally. Without an installed provider this resolves
    // the ambiguous process default and panics; with one pinned it returns a builder.
    let _config = ClientConfig::builder()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();

    // Calling it again is a no-op and must not panic.
    install_default_crypto_provider();
}
