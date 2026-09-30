use harnx_nats_common::connect::{parse_replicas_env, NatsEndpoint};

fn endpoint() -> NatsEndpoint {
    NatsEndpoint {
        name: "probe".into(),
        url: "tls://localhost:4222".into(),
        tls: Some(true),
        ..Default::default()
    }
}

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

#[test]
fn rejects_client_cert_without_key() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.tls_cert = Some("/tmp/client-cert.pem".into());
    let error = ep.connect_options().expect_err("should reject");
    assert!(error.to_string().contains("tls_key"), "got: {error}");
}

#[test]
fn rejects_missing_tls_file() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.tls_cert = Some("/definitely/missing-cert.pem".into());
    ep.tls_key = Some("/definitely/missing-key.pem".into());
    let error = ep.connect_options().expect_err("should reject");
    assert!(error.to_string().contains("does not exist"), "got: {error}");
}

#[test]
fn from_env_reads_url_and_token() {
    harnx_core::require_nextest();
    // Set via a single-threaded test to avoid cross-test env races; nextest
    // gives each test its own process, so this is safe here.
    std::env::set_var("HARNX_NATS_URL", "nats://127.0.0.1:4222");
    std::env::set_var("HARNX_NATS_TOKEN", "secret");
    std::env::remove_var("HARNX_NATS_REPLICAS");
    let ep = NatsEndpoint::from_env().expect("read env");
    assert_eq!(ep.url, "nats://127.0.0.1:4222");
    assert_eq!(ep.token.as_deref(), Some("secret"));
    assert_eq!(ep.tls_ca, None);
}

#[test]
fn parse_replicas_env_is_none_when_unset() {
    harnx_core::require_nextest();
    std::env::remove_var("HARNX_NATS_REPLICAS");
    assert_eq!(parse_replicas_env().expect("unset is not an error"), None);
}

#[test]
fn parse_replicas_env_accepts_a_valid_count() {
    harnx_core::require_nextest();
    std::env::set_var("HARNX_NATS_REPLICAS", "3");
    assert_eq!(parse_replicas_env().expect("valid count"), Some(3));
    std::env::remove_var("HARNX_NATS_REPLICAS");
}

#[test]
fn parse_replicas_env_rejects_an_unparseable_value_instead_of_defaulting() {
    harnx_core::require_nextest();
    std::env::set_var("HARNX_NATS_REPLICAS", "3x");
    let error = parse_replicas_env().expect_err("a typo must not silently become 1 replica");
    assert!(
        error.to_string().contains("HARNX_NATS_REPLICAS"),
        "got: {error}"
    );
    std::env::remove_var("HARNX_NATS_REPLICAS");
}

/// `build_custom_tls_client_config` used to call
/// `async_nats::rustls::ClientConfig::builder()`, which resolves rustls'
/// process-default `CryptoProvider`. This workspace links both `ring` (via
/// async-nats) and `aws-lc-rs` (via the AWS SDK stack), so no crate-feature
/// default is unambiguous and nothing in production calls
/// `CryptoProvider::install_default` — the combination `docs/nats-ha.md`
/// recommends (`tls_ca` for a custom CA) panicked instead of returning an
/// error. This test builds a real CA PEM and exercises exactly that
/// configuration.
#[test]
fn connect_options_succeeds_with_a_custom_tls_ca() {
    harnx_core::require_nextest();
    let ca = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate a self-signed CA cert for the test");
    let dir = tempfile::tempdir().expect("create temp dir for the CA PEM");
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, ca.cert.pem()).expect("write CA PEM");

    let mut ep = endpoint();
    ep.tls_ca = Some(ca_path.to_string_lossy().into_owned());

    let options = ep.connect_options();
    assert!(
        options.is_ok(),
        "connect_options() with tls_ca set must not panic and must succeed, got: {:?}",
        options.err()
    );
}

/// `connect()` moves the `tls_ca` file read into `spawn_blocking` so it
/// doesn't block a Tokio runtime worker thread; this checks that a missing
/// CA path still surfaces as an error naming the path, not a panic or a
/// silent success, once routed through that blocking task.
#[tokio::test]
async fn connect_rejects_a_missing_tls_ca_without_blocking_the_runtime() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.url = "tls://127.0.0.1:0".into();
    ep.tls_ca = Some("/definitely/missing-ca.pem".into());

    let error = ep.connect().await.expect_err("missing tls_ca must error");
    let message = error.to_string();
    assert!(message.contains("missing-ca.pem"), "got: {message}");
    assert!(message.contains("does not exist"), "got: {message}");
}

#[test]
fn from_env_rejects_an_unparseable_replicas_value() {
    harnx_core::require_nextest();
    std::env::set_var("HARNX_NATS_URL", "nats://127.0.0.1:4222");
    std::env::set_var("HARNX_NATS_TOKEN", "secret");
    std::env::set_var("HARNX_NATS_REPLICAS", "3x");
    let error = NatsEndpoint::from_env().expect_err("a typo must not silently become 1 replica");
    assert!(
        error.to_string().contains("HARNX_NATS_REPLICAS"),
        "got: {error}"
    );
    std::env::remove_var("HARNX_NATS_REPLICAS");
}

/// A `ws://` connection is plaintext end to end: async-nats reads TLS off the
/// scheme and never runs the NATS protocol's TLS upgrade on a WebSocket. An
/// operator who pairs `tls: true` with `ws://` would otherwise get an
/// unencrypted connection while the config says the opposite.
#[test]
fn rejects_tls_true_on_a_plaintext_websocket_url() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.url = "ws://localhost:8080".into();
    let error = ep.connect_options().expect_err("should reject");
    assert!(error.to_string().contains("wss://"), "got: {error}");
}

/// Same downgrade, reached through a client certificate rather than the `tls`
/// flag — the certificate an ALB would check would simply never be sent.
#[test]
fn rejects_a_client_certificate_on_a_plaintext_websocket_url() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.url = "ws://localhost:8080".into();
    ep.tls = None;
    ep.tls_cert = Some("/tmp/client-cert.pem".into());
    ep.tls_key = Some("/tmp/client-key.pem".into());
    let error = ep.connect_options().expect_err("should reject");
    assert!(error.to_string().contains("wss://"), "got: {error}");
}

/// A plain `ws://` cluster with no TLS settings is a legitimate
/// configuration — a broker behind a load balancer that terminates TLS, on a
/// trusted network leg.
#[test]
fn accepts_a_plaintext_websocket_url_without_tls_settings() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.url = "ws://localhost:8080".into();
    ep.tls = None;
    ep.connect_options()
        .expect("a ws:// URL with no TLS settings is valid");
}

/// The mTLS-through-an-ALB shape: `wss://` plus a client certificate, which
/// async-nats feeds into the same rustls config it uses for `tls://`.
#[test]
fn accepts_a_client_certificate_on_a_secure_websocket_url() {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir().expect("create temp dir");
    let (cert, key) = write_cert_and_key(dir.path(), "client");

    let mut ep = endpoint();
    ep.url = "wss://nats.example.com:443".into();
    ep.tls = None;
    ep.tls_cert = Some(cert);
    ep.tls_key = Some(key);

    ep.connect_options()
        .expect("wss:// with a client certificate is the supported mTLS shape");
}

/// A private CA for the broker's own certificate and a client certificate for
/// mTLS are independent needs, and a deployment that runs its own PKI has
/// both. They used to be rejected together because a custom root store
/// replaced the config async-nats built the client certificate into; they are
/// now built into one rustls config instead.
#[test]
fn accepts_tls_ca_together_with_a_client_certificate() {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir().expect("create temp dir");
    let (ca, _) = write_cert_and_key(dir.path(), "ca");
    let (cert, key) = write_cert_and_key(dir.path(), "client");

    let mut ep = endpoint();
    ep.tls_ca = Some(ca);
    ep.tls_cert = Some(cert);
    ep.tls_key = Some(key);

    ep.connect_options()
        .expect("a private CA and a client certificate must be usable together");
}

/// A client certificate whose key does not match it cannot be reported at
/// handshake time — rustls rejects the pair while the config is built, and
/// that error has to name both paths rather than surface as a panic.
#[test]
fn rejects_a_client_certificate_whose_key_does_not_match() {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir().expect("create temp dir");
    let (ca, _) = write_cert_and_key(dir.path(), "ca");
    let (cert, _) = write_cert_and_key(dir.path(), "client");
    let (_, other_key) = write_cert_and_key(dir.path(), "unrelated");

    let mut ep = endpoint();
    ep.tls_ca = Some(ca);
    ep.tls_cert = Some(cert);
    ep.tls_key = Some(other_key);

    let error = ep
        .connect_options()
        .expect_err("mismatched pair must error");
    assert!(error.to_string().contains("tls_cert"), "got: {error}");
}

/// Behind a WebSocket entry point the peers a clustered broker advertises are
/// raw `nats://` addresses reachable only from inside the cluster, so
/// following them would abandon the load balancer that gates access.
#[test]
fn websocket_urls_ignore_discovered_servers_by_default() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.tls = None;
    ep.url = "wss://nats.example.com:443".into();
    assert!(ep.resolved_ignore_discovered_servers());
    ep.url = "ws://localhost:8080".into();
    assert!(ep.resolved_ignore_discovered_servers());
}

/// On TCP, discovery is how a client finds the rest of the cluster, so the
/// default must stay the opposite way round.
#[test]
fn tcp_urls_honour_discovered_servers_by_default() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    assert!(!ep.resolved_ignore_discovered_servers());
    ep.url = "nats://localhost:4222".into();
    assert!(!ep.resolved_ignore_discovered_servers());
}

/// Both defaults are overridable: a WebSocket cluster whose advertised peers
/// really are reachable, and a TCP cluster behind a single address that should
/// not be traded for whatever the broker advertises.
#[test]
fn an_explicit_discovery_setting_overrides_the_transport_default() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.tls = None;
    ep.url = "wss://nats.example.com:443".into();
    ep.ignore_discovered_servers = Some(false);
    assert!(!ep.resolved_ignore_discovered_servers());

    ep.url = "nats://localhost:4222".into();
    ep.ignore_discovered_servers = Some(true);
    assert!(ep.resolved_ignore_discovered_servers());
}

/// The override reaches a standalone tool/hook server the same way every other
/// broker setting does, so a worker and the children it spawns agree.
#[test]
fn from_env_reads_the_discovery_override() {
    harnx_core::require_nextest();
    std::env::set_var("HARNX_NATS_URL", "wss://nats.example.com:443");
    std::env::remove_var("HARNX_NATS_REPLICAS");
    std::env::set_var("HARNX_NATS_IGNORE_DISCOVERED_SERVERS", "false");
    let ep = NatsEndpoint::from_env().expect("read env");
    assert_eq!(ep.ignore_discovered_servers, Some(false));
    assert!(!ep.resolved_ignore_discovered_servers());
    std::env::remove_var("HARNX_NATS_IGNORE_DISCOVERED_SERVERS");
}
