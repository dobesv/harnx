//! Crypto-provider resolution for TLS broker connections.
//!
//! rustls' `ClientConfig::builder()` resolves the *process-default*
//! `CryptoProvider`, which panics when more than one provider feature is
//! compiled in and nothing has installed a default. That ambiguity is real in
//! this workspace: `cargo tree -p harnx-runtime -e features -i rustls` shows
//! rustls built with both `ring` (via async-nats/tokio-rustls) and `aws-lc-rs`
//! (via the AWS SDK's hyper-rustls stack), and no production binary installs a
//! default.
//!
//! These tests must live here rather than beside the code they cover.
//! `harnx-nats-common` on its own resolves rustls with `ring` alone, so the
//! same assertions pass there no matter what the connect path does — the bug
//! only exists in a graph that also pulls in the AWS SDK. `harnx-runtime`,
//! `harnx-worker` and `harnx` are all such graphs.
//!
//! A `wss://` URL builds its TLS config before any network I/O, which makes
//! every one of these reachable without a live broker: nothing listens on the
//! port, so the connection must fail with that refusal rather than a panic.

use harnx_nats_common::connect::NatsEndpoint;

/// A self-signed cert and its key on disk, usable as either a CA bundle or a
/// client certificate.
fn write_cert_and_key(dir: &std::path::Path, stem: &str) -> (String, String) {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate a self-signed cert for the test");
    let cert_path = dir.join(format!("{stem}-cert.pem"));
    let key_path = dir.join(format!("{stem}-key.pem"));
    std::fs::write(&cert_path, generated.cert.pem()).expect("write cert PEM");
    std::fs::write(&key_path, generated.signing_key.serialize_pem()).expect("write key PEM");
    (
        cert_path.to_string_lossy().into_owned(),
        key_path.to_string_lossy().into_owned(),
    )
}

fn endpoint() -> NatsEndpoint {
    NatsEndpoint {
        name: "probe".into(),
        url: "wss://127.0.0.1:1".into(),
        ..Default::default()
    }
}

/// Connect far enough to build the TLS config, and report what happened
/// instead of it. Any panic here is the provider-resolution failure.
async fn connect_failure(endpoint: &NatsEndpoint) -> String {
    endpoint
        .connect_options()
        .expect("build connect options")
        .connect(&endpoint.url)
        .await
        .expect_err("nothing is listening on this port")
        .to_string()
}

/// The headline mTLS shape: a client certificate against a broker (or load
/// balancer) whose own certificate chains to a publicly-trusted CA, so no
/// `tls_ca` is set.
#[tokio::test]
async fn a_client_certificate_without_a_custom_ca_resolves_a_crypto_provider() {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir().expect("create temp dir");
    let (cert, key) = write_cert_and_key(dir.path(), "client");

    let mut ep = endpoint();
    ep.tls_cert = Some(cert);
    ep.tls_key = Some(key);

    assert!(!connect_failure(&ep).await.is_empty());
}

/// The most ordinary TLS configuration there is: no client certificate and no
/// custom CA, trusting the platform's roots.
#[tokio::test]
async fn plain_tls_without_any_certificates_resolves_a_crypto_provider() {
    harnx_core::require_nextest();
    let ep = endpoint();
    assert!(!connect_failure(&ep).await.is_empty());
}

/// A custom CA takes the path that already built its own config, with the
/// provider named explicitly.
#[tokio::test]
async fn a_custom_ca_resolves_a_crypto_provider() {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir().expect("create temp dir");
    let (ca, _) = write_cert_and_key(dir.path(), "ca");

    let mut ep = endpoint();
    ep.tls_ca = Some(ca);

    assert!(!connect_failure(&ep).await.is_empty());
}
