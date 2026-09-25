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
//! the explicit-TLS cases reachable without a live broker: nothing listens on
//! the port, so the connection must fail with that refusal rather than a panic.
//! The server-required upgrade case uses a small protocol stub because the
//! client learns that TLS is needed only after reading the server's INFO.

use harnx_nats_common::connect::NatsEndpoint;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// A plaintext TCP endpoint has no reason to require TLS up front, but the
/// broker can still demand an upgrade in its INFO. async-nats otherwise builds
/// a rustls config with `ClientConfig::builder()` at that point, which panics
/// in this crate's deliberately ambiguous provider graph.
#[tokio::test]
async fn server_required_tls_upgrade_resolves_a_crypto_provider() {
    harnx_core::require_nextest();
    assert!(
        async_nats::rustls::crypto::CryptoProvider::get_default().is_none(),
        "test requires the same unset process default as production"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind protocol stub");
    let address = listener.local_addr().expect("read protocol stub address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept NATS client");
        stream
            .write_all(b"INFO {\"tls_required\":true}\r\n")
            .await
            .expect("advertise required TLS upgrade");

        let mut first_tls_byte = [0_u8; 1];
        stream
            .read_exact(&mut first_tls_byte)
            .await
            .expect("read start of TLS handshake");
        first_tls_byte[0]
    });

    let endpoint = NatsEndpoint {
        name: "server-required-upgrade".into(),
        url: format!("nats://{address}"),
        ..Default::default()
    };
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        endpoint
            .connect_options()
            .expect("build options for plaintext TCP endpoint")
            .connect(&endpoint.url),
    )
    .await
    .expect("connection attempt must finish")
    .expect_err("protocol stub cannot complete a TLS handshake");

    assert!(!error.to_string().is_empty());
    assert_eq!(
        server.await.expect("protocol stub must finish"),
        0x16,
        "client must start a TLS handshake after the INFO"
    );
}
