use super::*;
use axum::http::HeaderMap;
use clap::Parser;

fn args(dir: &std::path::Path) -> cli::Args {
    cli::Args::parse_from([
        "harnx-a2a-server",
        "--agent",
        "missing-agent",
        "--user-id-header",
        "x-user",
        "--config-dir",
        dir.to_str().unwrap(),
    ])
}

#[test]
fn aliases_startup_missing_config_preserves_singleton() -> Result<()> {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir()?;
    std::env::set_var("HARNX_CONFIG_DIR", dir.path());
    let identity = startup_identity(&args(dir.path()))?;
    let mut headers = HeaderMap::new();
    headers.insert("x-user", "alice".parse()?);
    let request = identity.resolve_request(&headers).unwrap();
    assert_eq!(request.caller().view().users, &["alice"]);
    Ok(())
}

#[test]
fn aliases_startup_keeps_loaded_snapshot() -> Result<()> {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir()?;
    std::env::set_var("HARNX_CONFIG_DIR", dir.path());
    let path = dir.path().join("users.yaml");
    std::fs::write(&path, "- name: Person\n  identities: [alice, bob]\n")?;
    let identity = startup_identity(&args(dir.path()))?;
    std::fs::write(&path, "not: a sequence\n")?;
    let mut headers = HeaderMap::new();
    headers.insert("x-user", "alice".parse()?);
    for policy in [identity.clone(), identity] {
        let request = policy.resolve_request(&headers).unwrap();
        assert_eq!(request.principal.user_id(), Some("alice"));
        assert_eq!(request.caller().view().users, &["alice", "bob"]);
    }
    Ok(())
}

#[tokio::test]
async fn aliases_startup_malformed_config_fails_before_export_resolution() -> Result<()> {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("users.yaml");
    for yaml in [
        "not: a sequence",
        "- name: missing-identities",
        "---",
        "[unclosed",
    ] {
        std::fs::write(&path, yaml)?;
        let error = run(args(dir.path())).await.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(path.to_str().unwrap()), "{message}");
        assert!(message.contains("user aliases"), "{message}");
        assert!(!message.contains("resolve agent exports"), "{message}");
    }
    Ok(())
}

fn bearer_args(dir: &std::path::Path, sources: &[&str], flags: &[&str]) -> cli::Args {
    let mut argv = vec![
        "harnx-a2a-server".to_owned(),
        "--agent".into(),
        "missing-agent".into(),
        "--config-dir".into(),
        dir.to_str().unwrap().to_owned(),
    ];
    for source in sources {
        argv.push("--user-id-header".into());
        argv.push((*source).into());
    }
    for flag in flags {
        argv.push((*flag).into());
    }
    cli::Args::parse_from(argv)
}

#[test]
fn bearer_verification_flags_are_required_together() -> Result<()> {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir()?;
    std::env::set_var("HARNX_CONFIG_DIR", dir.path());
    for flags in [
        &["--bearer-issuer", "forge/invocation-token"][..],
        &["--bearer-audience", "ari:cloud:ecosystem::app/app-id"][..],
        &[
            "--bearer-jwks-url",
            "https://example.com/.well-known/jwks.json",
        ][..],
        &[
            "--bearer-jwks-url",
            "https://example.com/.well-known/jwks.json",
            "--bearer-issuer",
            "forge/invocation-token",
        ][..],
        &[
            "--bearer-jwks-url",
            "https://example.com/.well-known/jwks.json",
            "--bearer-audience",
            "ari:cloud:ecosystem::app/app-id",
        ][..],
    ] {
        let error = startup_identity(&bearer_args(dir.path(), &["forge-fit"], flags)).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("--bearer-jwks-url, --bearer-issuer and --bearer-audience"),
            "{message}"
        );
    }
    Ok(())
}

#[test]
fn bearer_verification_requires_a_bearer_source() -> Result<()> {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir()?;
    std::env::set_var("HARNX_CONFIG_DIR", dir.path());
    let flags = [
        "--bearer-jwks-url",
        "https://example.com/.well-known/jwks.json",
        "--bearer-issuer",
        "forge/invocation-token",
        "--bearer-audience",
        "ari:cloud:ecosystem::app/app-id",
    ];
    let error = startup_identity(&bearer_args(dir.path(), &["x-user"], &flags)).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("bearer verification flags"), "{message}");
    // A bearer source accepts the same flags.
    startup_identity(&bearer_args(dir.path(), &["forge-fit"], &flags))?;
    Ok(())
}

#[test]
fn bearer_verification_flags_off_keeps_decode_only_identity() -> Result<()> {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir()?;
    std::env::set_var("HARNX_CONFIG_DIR", dir.path());
    let identity = startup_identity(&bearer_args(dir.path(), &["forge-fit"], &[]))?;
    // No verifier: a bearer token decodes without verification.
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer e30.eyJwcmluY2lwYWwiOiJhY2NvdW50LWlkIiwiY29udGV4dCI6eyJjbG91ZElkIjoic2l0ZS1pZCJ9fQ.sig"
            .parse()?,
    );
    assert_eq!(
        identity
            .resolve_request(&headers)
            .unwrap()
            .principal
            .user_id(),
        Some("CN=account-id,OU=site-id,O=atlassian.com")
    );
    Ok(())
}

#[test]
fn bearer_verification_rejects_http_jwks_url() -> Result<()> {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir()?;
    std::env::set_var("HARNX_CONFIG_DIR", dir.path());
    for url in [
        "http://example.com/.well-known/jwks.json",
        "http://forge.cdn.prod.atlassian-dev.net/.well-known/jwks.json",
    ] {
        let flags = [
            "--bearer-jwks-url",
            url,
            "--bearer-issuer",
            "forge/invocation-token",
            "--bearer-audience",
            "ari:cloud:ecosystem::app/app-id",
        ];
        let error = startup_identity(&bearer_args(dir.path(), &["forge-fit"], &flags)).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("https; http is only allowed on loopback"),
            "{message}"
        );
    }
    Ok(())
}

#[test]
fn bearer_verification_rejects_empty_issuer_or_audience() -> Result<()> {
    harnx_core::require_nextest();
    let dir = tempfile::tempdir()?;
    std::env::set_var("HARNX_CONFIG_DIR", dir.path());
    for (empty, name) in [
        ([("--bearer-issuer", "")], "--bearer-issuer"),
        ([("--bearer-issuer", " \t")], "--bearer-issuer"),
        ([("--bearer-audience", "")], "--bearer-audience"),
        ([("--bearer-audience", "  ")], "--bearer-audience"),
    ] {
        let mut flags = vec![
            "--bearer-jwks-url",
            "https://example.com/.well-known/jwks.json",
        ];
        for (flag, value) in &empty {
            flags.push(flag);
            flags.push(value);
        }
        if name == "--bearer-issuer" {
            flags.push("--bearer-audience");
            flags.push("ari:cloud:ecosystem::app/app-id");
        } else {
            flags.push("--bearer-issuer");
            flags.push("forge/invocation-token");
        }
        let error = startup_identity(&bearer_args(dir.path(), &["forge-fit"], &flags)).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains(&format!("{name} must not be empty")),
            "{message}"
        );
    }
    Ok(())
}
