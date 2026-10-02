use crate::config::test_support::{env_lock_async, EnvGuard};
use crate::config::*;

/// Regression test for the non-interactive failure where `use_agent_by_name`
/// followed by `use_session` bailed with "agent variables are required"
/// for an agent whose variables use `path:` (file-backed defaults).  The
/// async `agent::init` resolves these defaults, but the synchronous
/// `retrieve_agent` does not — `use_agent_by_name` must do so itself,
/// otherwise `init_agent_session_variables` (called from `use_session`)
/// finds no defaults and bails in non-interactive contexts.
#[tokio::test]
async fn test_use_agent_by_name_resolves_file_backed_variable_defaults() {
    use crate::client::TestStateGuard;

    let temp = tempfile::TempDir::new().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(agents_dir.join("shared")).unwrap();
    std::fs::write(
            agents_dir.join("file-backed-vars.md"),
            "---\nvariables:\n  - name: prompt_body\n    description: Shared prompt\n    path: shared/prompt.md\n---\n{{prompt_body}}\n",
        )
        .unwrap();
    std::fs::write(agents_dir.join("shared/prompt.md"), "Loaded body").unwrap();

    // Hold the global test lock so concurrent tests can't race on the
    // shared HARNX_CONFIG_DIR env var.
    let _guard = TestStateGuard::new(None).await;
    let _env_lock = env_lock_async().await;
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());

    // Drive use_session in non-interactive mode so the inquire prompt
    // that would otherwise hang in CI is suppressed.  The fix must still
    // produce populated shared_variables under no_interaction.
    let mut config = Config {
        info_flag: true,
        ..Default::default()
    };
    config
        .use_agent_by_name("file-backed-vars")
        .expect("use_agent_by_name must resolve path-backed variable defaults");
    config
        .use_session(Some("file-backed-vars-session"))
        .expect("use_session must succeed once defaults are resolved");

    let agent = config.agent.as_ref().expect("agent should be set");
    assert_eq!(
        agent
            .shared_variables()
            .get("prompt_body")
            .map(String::as_str),
        Some("Loaded body"),
        "shared_variables should be populated from the file-backed default"
    );
}

#[tokio::test]
async fn use_agent_routes_remote_refs_to_nats_cluster_validation() {
    use crate::client::TestStateGuard;
    use harnx_core::{abort::create_abort_signal, working_mode::WorkingMode};

    let _guard = TestStateGuard::new(None).await;
    let _env_lock = env_lock_async().await;

    let temp = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let _provider = EnvGuard::new("HARNX_PROVIDER", "claude:some-model");

    let config = Config::init(WorkingMode::Cmd, false)
        .await
        .expect("config init");
    let config = std::sync::Arc::new(crate::config::ConfigLock::new(config));

    let err = Config::use_agent(&config, "atlas@prod", None, create_abort_signal())
        .await
        .expect_err("remote ref with an unknown cluster must fail cluster validation");

    // Remote refs are no longer stubbed out — activation validates the cluster
    // first. With no nats_servers/prod.yaml the activation fails on cluster
    // lookup, proving the path is wired.
    let msg = err.to_string();
    assert!(
        msg.contains("prod") && msg.contains("nats_servers/prod.yaml"),
        "expected unknown-cluster validation error, got: {msg}"
    );
    assert!(
        !msg.contains("not yet implemented"),
        "remote refs must no longer be stubbed: {msg}"
    );
}
