//! NATS cluster config loading and connection helpers.
use super::*;
use anyhow::{bail, Context, Result};
use async_nats::jetstream;
use harnx_core::agent_config::AgentRole;
use harnx_nats_common::connect::{
    NatsEndpoint, HARNX_NATS_TLS_CA_ENV, HARNX_NATS_TLS_CERT_ENV, HARNX_NATS_TLS_ENV,
    HARNX_NATS_TLS_KEY_ENV,
};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteAgentEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub role: AgentRole,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NatsServerConfig {
    #[serde(default)]
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// JetStream replica count for buckets harnx creates on this cluster.
    /// Defaults to 1 when absent; see `docs/nats-ha.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<RemoteAgentEntry>,
}

impl NatsServerConfig {
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }

    /// The JetStream replica count to actually use for this cluster's
    /// buckets: the configured value, or 1 when unset.
    pub fn resolved_replicas(&self) -> usize {
        self.replicas.unwrap_or(1)
    }
}

/// Resolve connection details for the reserved shared-local cluster.
///
/// Front-ends and worker subprocesses use this helper as their single source
/// of dynamic config. A complete environment handoff takes precedence; without
/// one, details come from the auto-managed shared broker.
pub async fn resolve_local_nats_server_config() -> Result<NatsServerConfig> {
    let (url, token, replicas, tls, tls_cert, tls_key, tls_ca) = match (
        std::env::var(HARNX_NATS_URL_ENV).ok(),
        std::env::var(HARNX_NATS_TOKEN_ENV).ok(),
    ) {
        (Some(url), Some(token)) => {
            // Only a complete handoff can mean "this is a real cluster an
            // operator configured"; the auto-managed broker below is always a
            // single embedded, TLS-less process, so it never reads any of this.
            let replicas = std::env::var(HARNX_NATS_REPLICAS_ENV)
                .ok()
                .and_then(|value| value.parse().ok());
            // Same env var names as `NatsEndpoint::from_env`: a worker
            // resolving its own discovery client and the child tool/hook
            // servers it spawns must agree on TLS, or a worker on a TLS
            // cluster spawns children that can't reach the broker.
            let tls = std::env::var(HARNX_NATS_TLS_ENV)
                .ok()
                .map(|value| value == "1" || value == "true");
            let tls_cert = std::env::var(HARNX_NATS_TLS_CERT_ENV).ok();
            let tls_key = std::env::var(HARNX_NATS_TLS_KEY_ENV).ok();
            let tls_ca = std::env::var(HARNX_NATS_TLS_CA_ENV).ok();
            (url, token, replicas, tls, tls_cert, tls_key, tls_ca)
        }
        (None, None) => {
            let server = crate::nats_local_server::ensure_shared_server().await?;
            (
                server.url.clone(),
                server.token.clone(),
                None,
                None,
                None,
                None,
                None,
            )
        }
        _ => bail!("{HARNX_NATS_URL_ENV} and {HARNX_NATS_TOKEN_ENV} must be set together"),
    };

    Ok(NatsServerConfig {
        name: LOCAL_CLUSTER_KEY.to_string(),
        url,
        token: Some(token),
        replicas,
        tls,
        tls_cert,
        tls_key,
        tls_ca,
        agents: vec![],
    })
}

impl Config {
    pub fn load_nats_servers_from_dir(dir: &Path) -> Result<Vec<NatsServerConfig>> {
        if !dir.exists() {
            return Ok(vec![]);
        }

        let mut servers = Vec::new();
        for path in Self::sorted_yaml_files(dir)? {
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(stem) if !stem.is_empty() => stem.to_string(),
                _ => continue,
            };

            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read NATS server config {}", path.display()))?;
            let mut server: NatsServerConfig =
                serde_yaml::from_str(&content).with_context(|| {
                    format!("Failed to parse NATS server config {}", path.display())
                })?;
            server.set_name(stem);
            Self::expand_nats_server_envs(&mut server);
            servers.push(server);
        }

        Ok(servers)
    }

    pub fn nats_server(&self, cluster_key: &str) -> Result<&NatsServerConfig> {
        self.nats_servers
            .iter()
            .find(|server| server.name == cluster_key)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown NATS cluster '{}' (expected nats_servers/{}.yaml)",
                    cluster_key,
                    cluster_key
                )
            })
    }

    /// Resolve one cluster's config, whether it's a reserved dynamic identity
    /// (`LOCAL_CLUSTER_KEY`) or a `nats_servers/<cluster_key>.yaml` entry.
    ///
    /// `pub(crate)` rather than private: `nats_worker::daemon` needs the
    /// resolved `replicas` value before it connects, not just a client.
    pub(crate) async fn resolve_nats_server<'a>(
        &'a self,
        cluster_key: &str,
    ) -> Result<Cow<'a, NatsServerConfig>> {
        if cluster_key == LOCAL_CLUSTER_KEY {
            // Reserved dynamic identity wins even if a file named
            // nats_servers/__local__.yaml was loaded.
            return Ok(Cow::Owned(resolve_local_nats_server_config().await?));
        }
        self.nats_server(cluster_key).map(Cow::Borrowed)
    }

    pub async fn nats_client(&self, cluster_key: &str) -> Result<async_nats::Client> {
        let server = self.resolve_nats_server(cluster_key).await?;
        Self::connect_nats_server(&server).await.with_context(|| {
            format!(
                "Failed to connect to NATS cluster '{}' at '{}'",
                cluster_key, server.url
            )
        })
    }

    pub async fn nats_jetstream(&self, cluster_key: &str) -> Result<jetstream::Context> {
        let client = self.nats_client(cluster_key).await?;
        Ok(jetstream::new(client))
    }

    pub async fn connect_nats_server(server: &NatsServerConfig) -> Result<async_nats::Client> {
        if !server.has_auth_or_tls_config() {
            return async_nats::connect(&server.url)
                .await
                .with_context(|| format!("Failed to connect to NATS cluster at '{}'", server.url));
        }

        let options = NatsEndpoint::from(server)
            .connect_options()
            .with_context(|| {
                format!("Invalid auth/TLS config for NATS cluster '{}'", server.name)
            })?;
        options
            .connect(&server.url)
            .await
            .with_context(|| format!("Failed to connect to NATS cluster at '{}'", server.url))
    }

    pub async fn nats_kv_bucket(
        &self,
        cluster_key: &str,
        bucket: &str,
    ) -> Result<jetstream::kv::Store> {
        let js = self.nats_jetstream(cluster_key).await?;
        js.get_key_value(bucket)
            .await
            .with_context(|| format!("Failed to open NATS KV bucket '{}'", bucket))
    }

    pub(super) fn expand_nats_server_envs(server: &mut NatsServerConfig) {
        server.url = expand_env_string(&server.url);
        expand_env_option(&mut server.token);
        expand_env_option(&mut server.tls_cert);
        expand_env_option(&mut server.tls_key);
        expand_env_option(&mut server.tls_ca);
    }
}

impl NatsServerConfig {
    fn has_auth_or_tls_config(&self) -> bool {
        self.token.is_some()
            || self.tls.unwrap_or(false)
            || self.tls_cert.is_some()
            || self.tls_key.is_some()
            || self.tls_ca.is_some()
    }
}

/// Convert a config-file cluster entry into the connect-options builder
/// shared with the standalone tool/hook servers.
impl From<&NatsServerConfig> for NatsEndpoint {
    fn from(server: &NatsServerConfig) -> Self {
        Self {
            name: server.name.clone(),
            url: server.url.clone(),
            token: server.token.clone(),
            replicas: server.replicas,
            tls: server.tls,
            tls_cert: server.tls_cert.clone(),
            tls_key: server.tls_key.clone(),
            tls_ca: server.tls_ca.clone(),
        }
    }
}

fn expand_env_option(value: &mut Option<String>) {
    if let Some(inner) = value {
        *inner = expand_env_string(inner);
    }
}

fn expand_env_string(value: &str) -> String {
    shellexpand::env(value).map_or_else(|_| value.to_string(), |expanded| expanded.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{env_lock, EnvGuard};

    /// Assert a resolved server matches the expected dynamic-local identity
    /// (reserved name, url, token) and routes through the authenticated path.
    /// Uses a single tuple comparison so the check reads as one assertion.
    fn assert_authenticated_local_server(server: &NatsServerConfig, url: &str, token: &str) {
        let actual = (
            server.name.as_str(),
            server.url.as_str(),
            server.token.as_deref(),
            server.has_auth_or_tls_config(),
        );
        assert_eq!(actual, (LOCAL_CLUSTER_KEY, url, Some(token), true));
    }

    /// Assert an error message mentions each expected substring. Keeps the
    /// per-test assertion blocks small and states intent in one call.
    fn assert_error_mentions(error: &str, expected: &[&str]) {
        for needle in expected {
            assert!(
                error.contains(needle),
                "expected error to mention {needle:?}, got: {error}"
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn local_cluster_env_handoff_builds_authenticated_dynamic_config() {
        harnx_core::require_nextest();
        let _lock = env_lock();
        let _url = EnvGuard::new(
            HARNX_NATS_URL_ENV,
            std::path::Path::new("nats://127.0.0.1:4555"),
        );
        let _token = EnvGuard::new(HARNX_NATS_TOKEN_ENV, std::path::Path::new("handoff-token"));

        let server = resolve_local_nats_server_config().await.unwrap();

        assert_authenticated_local_server(&server, "nats://127.0.0.1:4555", "handoff-token");
    }

    /// A worker on a TLS cluster must be able to discover tool/hook servers
    /// over TLS too: `resolve_local_nats_server_config` backs
    /// `NatsToolProvider`/`NatsHookProvider` discovery (both call
    /// `config.nats_client(LOCAL_CLUSTER_KEY)`), so it must read the same
    /// `HARNX_NATS_TLS*` variables `NatsEndpoint::from_env` does, or that
    /// discovery silently stays plaintext regardless of cluster config.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn local_cluster_env_handoff_carries_tls_settings() {
        harnx_core::require_nextest();
        let _lock = env_lock();
        let _url = EnvGuard::new(
            HARNX_NATS_URL_ENV,
            std::path::Path::new("tls://127.0.0.1:4555"),
        );
        let _token = EnvGuard::new(HARNX_NATS_TOKEN_ENV, std::path::Path::new("handoff-token"));
        let _tls = EnvGuard::new("HARNX_NATS_TLS", std::path::Path::new("true"));
        let _cert = EnvGuard::new(
            "HARNX_NATS_TLS_CERT",
            std::path::Path::new("/tmp/client-cert.pem"),
        );
        let _key = EnvGuard::new(
            "HARNX_NATS_TLS_KEY",
            std::path::Path::new("/tmp/client-key.pem"),
        );
        let _ca = EnvGuard::new("HARNX_NATS_TLS_CA", std::path::Path::new("/tmp/ca.pem"));

        let server = resolve_local_nats_server_config().await.unwrap();

        let actual = (
            server.tls,
            server.tls_cert.as_deref(),
            server.tls_key.as_deref(),
            server.tls_ca.as_deref(),
        );
        assert_eq!(
            actual,
            (
                Some(true),
                Some("/tmp/client-cert.pem"),
                Some("/tmp/client-key.pem"),
                Some("/tmp/ca.pem"),
            )
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn local_cluster_dynamic_resolution_wins_over_reserved_yaml_entry() {
        harnx_core::require_nextest();
        let _lock = env_lock();
        let _url = EnvGuard::new(
            HARNX_NATS_URL_ENV,
            std::path::Path::new("nats://127.0.0.1:4666"),
        );
        let _token = EnvGuard::new(HARNX_NATS_TOKEN_ENV, std::path::Path::new("dynamic-token"));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("__local__.yaml"),
            "url: nats://static.invalid:4222\ntoken: static-token\n",
        )
        .unwrap();
        let config = Config {
            nats_servers: Config::load_nats_servers_from_dir(directory.path()).unwrap(),
            ..Config::default()
        };
        assert_eq!(
            config.nats_server(LOCAL_CLUSTER_KEY).unwrap().url,
            "nats://static.invalid:4222"
        );

        let server = config.resolve_nats_server(LOCAL_CLUSTER_KEY).await.unwrap();

        assert_eq!(server.url, "nats://127.0.0.1:4666");
        assert_eq!(server.token.as_deref(), Some("dynamic-token"));
        assert!(matches!(server, Cow::Owned(_)));
    }

    #[test]
    fn expand_nats_server_envs_expands_secret_fields() {
        let _lock = env_lock();
        let _token = EnvGuard::new("NATS_TOKEN", std::path::Path::new("secret-token"));
        let _cert = EnvGuard::new("NATS_CERT", std::path::Path::new("/tmp/client-cert.pem"));
        let _key = EnvGuard::new("NATS_KEY", std::path::Path::new("/tmp/client-key.pem"));
        let _ca = EnvGuard::new("NATS_CA", std::path::Path::new("/tmp/ca.pem"));

        let mut server = NatsServerConfig {
            name: "local".into(),
            url: "nats://${NATS_TOKEN}@localhost:4222".into(),
            token: Some("${NATS_TOKEN}".into()),
            replicas: None,
            tls: Some(true),
            tls_cert: Some("${NATS_CERT}".into()),
            tls_key: Some("${NATS_KEY}".into()),
            tls_ca: Some("${NATS_CA}".into()),
            agents: vec![],
        };
        Config::expand_nats_server_envs(&mut server);

        assert_eq!(server.url, "nats://secret-token@localhost:4222");
        assert_eq!(server.token.as_deref(), Some("secret-token"));
        assert_eq!(server.tls_cert.as_deref(), Some("/tmp/client-cert.pem"));
        assert_eq!(server.tls_key.as_deref(), Some("/tmp/client-key.pem"));
        assert_eq!(server.tls_ca.as_deref(), Some("/tmp/ca.pem"));
    }

    #[test]
    fn nats_server_config_parses_agents_list_with_description_and_role() {
        let config: NatsServerConfig = serde_yaml::from_str(
            r#"
url: nats://localhost:4222
agents:
  - name: forge-atlas
    description: Handles heavy planning
    role: subagent
"#,
        )
        .unwrap();

        assert_eq!(config.url, "nats://localhost:4222");
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].name, "forge-atlas");
        assert_eq!(
            config.agents[0].description.as_deref(),
            Some("Handles heavy planning")
        );
        assert_eq!(config.agents[0].role, AgentRole::Subagent);
    }

    #[test]
    fn nats_server_config_parses_agents_defaults_for_missing_fields() {
        let config: NatsServerConfig = serde_yaml::from_str(
            r#"
url: nats://localhost:4222
agents:
  - name: no-description
    role: subagent
  - name: no-role
    description: Defaults role to assistant
"#,
        )
        .unwrap();

        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.agents[0].name, "no-description");
        assert_eq!(config.agents[0].description, None);
        assert_eq!(config.agents[0].role, AgentRole::Subagent);
        assert_eq!(config.agents[1].name, "no-role");
        assert_eq!(
            config.agents[1].description.as_deref(),
            Some("Defaults role to assistant")
        );
        assert_eq!(config.agents[1].role, AgentRole::Assistant);
    }

    #[test]
    fn parses_replicas_from_cluster_config() {
        let server: NatsServerConfig =
            serde_yaml::from_str("url: nats://localhost:4222\nreplicas: 3\n")
                .expect("parse cluster config");
        assert_eq!(server.replicas, Some(3));
    }

    #[test]
    fn replicas_defaults_to_none_when_absent() {
        let server: NatsServerConfig =
            serde_yaml::from_str("url: nats://localhost:4222\n").expect("parse");
        assert_eq!(server.replicas, None);
    }

    #[test]
    fn resolved_replicas_defaults_to_one_when_absent() {
        let server: NatsServerConfig =
            serde_yaml::from_str("url: nats://localhost:4222\n").expect("parse");
        assert_eq!(server.resolved_replicas(), 1);
    }

    #[test]
    fn resolved_replicas_uses_configured_value() {
        let server: NatsServerConfig =
            serde_yaml::from_str("url: nats://localhost:4222\nreplicas: 3\n").expect("parse");
        assert_eq!(server.resolved_replicas(), 3);
    }

    #[test]
    fn nats_server_config_parses_without_agents_key_for_back_compat() {
        let config: NatsServerConfig = serde_yaml::from_str(
            r#"
url: nats://localhost:4222
tls: false
"#,
        )
        .unwrap();

        assert_eq!(config.url, "nats://localhost:4222");
        assert!(config.agents.is_empty());
    }

    // The TLS/mTLS rejection cases themselves now live with the builder in
    // `harnx-nats-common::connect` (see `connect_options.rs`); this test only
    // checks that `connect_nats_server` still routes through it correctly.
    #[tokio::test]
    async fn connect_nats_server_rejects_partial_client_cert_config() {
        harnx_core::require_nextest();
        let server = NatsServerConfig {
            name: "mtls".into(),
            url: "tls://localhost:4222".into(),
            token: None,
            replicas: None,
            tls: Some(true),
            tls_cert: Some("/tmp/client-cert.pem".into()),
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        };

        // `{:#}` prints the full anyhow chain; `connect_nats_server` wraps the
        // builder's error in its own "Invalid auth/TLS config" context, so a
        // plain `.to_string()` would only show that outer message.
        let error = format!(
            "{:#}",
            Config::connect_nats_server(&server).await.unwrap_err()
        );
        assert_error_mentions(&error, &["tls_cert", "tls_key", "mtls"]);
    }
}
