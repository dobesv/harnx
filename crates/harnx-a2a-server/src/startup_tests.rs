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
