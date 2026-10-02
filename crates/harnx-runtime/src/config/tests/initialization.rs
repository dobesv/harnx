use crate::config::test_support::{env_lock, env_lock_async, EnvGuard};
use crate::config::*;

async fn init_headless_fixture(
    config_yaml: &str,
    client: Option<(&str, &str)>,
) -> anyhow::Result<Config> {
    let _env_lock = env_lock_async().await;
    let temp = tempfile::tempdir()?;
    std::fs::write(temp.path().join("config.yaml"), config_yaml)?;
    if let Some((name, yaml)) = client {
        let clients_dir = temp.path().join("clients");
        std::fs::create_dir_all(&clients_dir)?;
        std::fs::write(clients_dir.join(format!("{name}.yaml")), yaml)?;
    }
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", temp.path());
    let _config_file = EnvGuard::remove("HARNX_CONFIG_FILE");
    let _model = EnvGuard::remove("HARNX_MODEL");
    Config::init_headless(WorkingMode::Cmd, false).await
}

#[tokio::test]
async fn headless_init_without_local_models_leaves_model_unset() {
    let config = init_headless_fixture("{}\n", None)
        .await
        .expect("model-less headless config should initialize");

    assert!(config.clients.is_empty());
    assert_eq!(config.current_model_id(), None);
}

#[tokio::test]
async fn headless_init_with_unknown_explicit_model_still_fails() {
    let error = init_headless_fixture("model: missing:chat\n", None)
        .await
        .expect_err("unknown explicit model should fail initialization");

    assert!(
        error.to_string().contains("missing:chat"),
        "error should identify the unknown model: {error:#}"
    );
}

#[tokio::test]
async fn headless_init_auto_selects_first_chat_model() {
    let config = init_headless_fixture(
        "{}\n",
        Some((
            "local",
            "type: openai-compatible\napi_base: http://localhost:1234\nmodels:\n  - name: test-chat\n    type: chat\n",
        )),
    )
    .await
    .expect("valid local client should initialize");

    assert_eq!(
        config.current_model_id().as_deref(),
        Some("local:test-chat")
    );
}
#[test]
fn dynamic_provider_model_init_sets_client_name_from_provider() {
    let _lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let _config_dir = EnvGuard::new("HARNX_CONFIG_DIR", tmp.path());
    let _provider = EnvGuard::new("HARNX_PROVIDER", "claude:some-model");

    let config = tokio_test::block_on(Config::init(WorkingMode::Cmd, false))
        .expect("dynamic config should load");

    assert_eq!(config.clients.len(), 1);
    assert_eq!(config.clients[0].effective_name(), "claude");
}
