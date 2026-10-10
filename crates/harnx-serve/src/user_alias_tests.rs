use crate::{
    request_identity::RequestIdentity, test_support::TestConfigSandbox, Server, StreamDrainConfig,
};
use anyhow::Result;
use harnx_core::{access_rules::AccessRules, config_paths::local_path};
use harnx_runtime::config::{ConfigLock, GlobalConfig};
use std::{path::PathBuf, sync::Arc};

fn config(sandbox: &TestConfigSandbox) -> GlobalConfig {
    harnx_core::require_nextest();
    let mut config = sandbox.config();
    config.serve_user_id_sources = vec!["x-user".into()];
    config.serve_group_headers = vec!["x-groups".into()];
    config.serve_role_headers = vec!["x-roles".into()];
    Arc::new(ConfigLock::new(config))
}

#[test]
fn aliases_embedded_fallible_constructors_reject_malformed_users() -> Result<()> {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox);
    let path = local_path("users.yaml");
    let rules = Arc::new(AccessRules::from_yaml(
        "rules:\n  - agents: ['*']\n    users: [bob]\n",
    )?);
    for yaml in [
        "---",
        "- name: missing-identities",
        "[unclosed",
        "users: []",
    ] {
        std::fs::write(&path, yaml)?;
        for enabled in [false, true] {
            let error = Server::new_with_access_rules(
                &config,
                PathBuf::new(),
                enabled.then(|| rules.clone()),
            )
            .err()
            .expect("invalid aliases must fail fallible constructor");
            let message = format!("{error:#}");
            assert!(message.contains(path.to_str().unwrap()), "{message}");
            assert!(message.contains("user aliases"), "{message}");
        }
        let error =
            Server::new_with_stream_drain(&config, PathBuf::new(), StreamDrainConfig::default())
                .err()
                .expect("invalid aliases must fail stream-drain constructor");
        assert!(format!("{error:#}").contains(path.to_str().unwrap()));
    }
    Ok(())
}

#[test]
fn aliases_embedded_server_new_panics_on_malformed_present_file() -> Result<()> {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox);
    std::fs::write(local_path("users.yaml"), "---")?;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Server::new(&config, PathBuf::new())
    }))
    .err()
    .expect("infallible Server::new must fail startup");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("panic message");
    assert!(message.contains("valid serve configuration"), "{message}");
    assert!(message.contains("users.yaml"), "{message}");
    Ok(())
}

#[test]
fn aliases_embedded_missing_config_and_loaded_snapshot() -> Result<()> {
    let sandbox = TestConfigSandbox::new();
    let config = config(&sandbox);
    let path = local_path("users.yaml");
    assert!(!path.try_exists()?);
    let without = Server::new(&config, PathBuf::new());
    assert!(without.user_aliases.is_none());
    std::fs::write(&path, "- name: Display\n  identities: [alice, bob]\n")?;
    let with = Server::new_with_access_rules(&config, PathBuf::new(), None)?;
    assert!(with.user_aliases.is_some());
    std::fs::write(&path, "[unclosed")?;
    let request = hyper::Request::builder()
        .header("x-user", "alice")
        .header("x-groups", "bob, team")
        .header("x-roles", "alice, operator")
        .body(())?;
    for (server, expected) in [(&without, vec!["alice"]), (&with, vec!["alice", "bob"])] {
        let identity = server.resolve_identity(&request)?;
        assert_eq!(identity.user_id.as_deref(), Some("alice"));
        let caller = identity.caller();
        assert_eq!(caller.view().users, expected);
        assert_eq!(caller.view().groups, &["bob", "team"]);
        assert_eq!(caller.view().roles, &["alice", "operator"]);
    }
    let anonymous = with.resolve_identity(&hyper::Request::new(()))?;
    assert!(anonymous.caller().view().users.is_empty());
    Ok(())
}

#[test]
fn aliases_literal_empty_user_is_distinct_from_anonymous_snapshot() {
    harnx_core::require_nextest();
    let empty = RequestIdentity {
        user_id: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(empty.caller().view().users, &[""]);
    let anonymous = RequestIdentity::default();
    assert!(anonymous.caller().view().users.is_empty());
}
