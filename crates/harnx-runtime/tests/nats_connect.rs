mod common;

use common::{spawn_nats_server, spawn_nats_server_with_options, SpawnNatsServerOptions};
use harnx_core::require_nextest;
use harnx_runtime::client::Model;
use harnx_runtime::config::Config;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

struct EnvGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }

    fn set_value(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn write_fixture_config(dir: &TempDir, cluster: &str, body: &str) {
    fs::create_dir_all(dir.path().join("nats_servers")).unwrap();
    fs::write(dir.path().join("config.yaml"), "save: false\n").unwrap();
    fs::write(
        dir.path()
            .join("nats_servers")
            .join(format!("{cluster}.yaml")),
        body,
    )
    .unwrap();
}

fn test_config_from_tmp(tmp: &TempDir) -> Config {
    Config {
        nats_servers: Config::load_nats_servers_from_dir(&tmp.path().join("nats_servers")).unwrap(),
        model: Model::new("test", "test-model"),
        ..Default::default()
    }
}

fn token_auth_config(server_url: &str, token: &str) -> (TempDir, EnvGuard, EnvGuard) {
    let tmp = tempfile::tempdir().unwrap();
    write_fixture_config(
        &tmp,
        "secure",
        &format!(
            "url: {}
token: ${{NATS_TEST_TOKEN}}
tls: false
",
            server_url
        ),
    );
    let config_guard = EnvGuard::set_path("HARNX_CONFIG_FILE", &tmp.path().join("config.yaml"));
    let token_guard = EnvGuard::set_value("NATS_TEST_TOKEN", token);
    (tmp, config_guard, token_guard)
}

#[tokio::test]
async fn nats_client_connects_and_jetstream_is_reachable() {
    require_nextest();

    let Some(server) = spawn_nats_server().await.unwrap() else {
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fixture_config(&tmp, "local", &format!("url: {}\n", server.url()));
    let _config_guard = EnvGuard::set_path("HARNX_CONFIG_FILE", &tmp.path().join("config.yaml"));

    let config = test_config_from_tmp(&tmp);
    let client = config.nats_client("local").await.unwrap();
    client.flush().await.unwrap();

    let _jetstream = config.nats_jetstream("local").await.unwrap();
}

#[tokio::test]
async fn nats_client_connects_with_token_auth_and_env_expansion() {
    require_nextest();

    let token = "s3cr3t-token";
    let Some(server) = spawn_nats_server_with_options(SpawnNatsServerOptions {
        auth_token: Some(token.to_string()),
    })
    .await
    .unwrap() else {
        return;
    };

    let (tmp, _config_guard, _token_guard) = token_auth_config(server.url(), token);

    let config = test_config_from_tmp(&tmp);
    let client = config.nats_client("secure").await.unwrap();
    client.flush().await.unwrap();
}

#[tokio::test]
async fn nats_client_fails_with_clear_error_when_token_wrong() {
    require_nextest();

    let token = "s3cr3t-token";
    let Some(server) = spawn_nats_server_with_options(SpawnNatsServerOptions {
        auth_token: Some(token.to_string()),
    })
    .await
    .unwrap() else {
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fixture_config(
        &tmp,
        "secure",
        &format!("url: {}\ntoken: wrong-token\n", server.url()),
    );
    let _config_guard = EnvGuard::set_path("HARNX_CONFIG_FILE", &tmp.path().join("config.yaml"));

    let config = test_config_from_tmp(&tmp);
    let error = config.nats_client("secure").await.unwrap_err().to_string();
    assert!(error.contains("Failed to connect to NATS cluster 'secure' at '"));
}
