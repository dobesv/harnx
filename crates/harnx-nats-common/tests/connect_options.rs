use harnx_nats_common::connect::NatsEndpoint;

fn endpoint() -> NatsEndpoint {
    NatsEndpoint {
        name: "probe".into(),
        url: "tls://localhost:4222".into(),
        token: None,
        replicas: None,
        tls: Some(true),
        tls_cert: None,
        tls_key: None,
        tls_ca: None,
    }
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
fn rejects_tls_ca_combined_with_a_client_certificate() {
    harnx_core::require_nextest();
    let mut ep = endpoint();
    ep.tls_cert = Some("/tmp/client-cert.pem".into());
    ep.tls_key = Some("/tmp/client-key.pem".into());
    ep.tls_ca = Some("/tmp/ca.pem".into());
    let error = ep.connect_options().expect_err("should reject");
    assert!(error.to_string().contains("not supported"), "got: {error}");
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
    let ep = NatsEndpoint::from_env().expect("read env");
    assert_eq!(ep.url, "nats://127.0.0.1:4222");
    assert_eq!(ep.token.as_deref(), Some("secret"));
    assert_eq!(ep.tls_ca, None);
}
