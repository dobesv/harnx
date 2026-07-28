//! NATS cluster config loading and connection helpers.
use super::*;
use anyhow::{bail, Context, Result};
use async_nats::{jetstream, ConnectOptions};
use harnx_core::agent_config::AgentRole;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    io::BufReader,
    path::{Path, PathBuf},
};

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
}

/// Resolve connection details for the reserved shared-local cluster.
///
/// Front-ends and worker subprocesses use this helper as their single source
/// of dynamic config. A complete environment handoff takes precedence; without
/// one, details come from the auto-managed shared broker.
pub async fn resolve_local_nats_server_config() -> Result<NatsServerConfig> {
    let (url, token) = match (
        std::env::var(HARNX_NATS_URL_ENV).ok(),
        std::env::var(HARNX_NATS_TOKEN_ENV).ok(),
    ) {
        (Some(url), Some(token)) => (url, token),
        (None, None) => {
            let server = crate::nats_local_server::ensure_shared_server().await?;
            (server.url.clone(), server.token.clone())
        }
        _ => bail!("{HARNX_NATS_URL_ENV} and {HARNX_NATS_TOKEN_ENV} must be set together"),
    };

    Ok(NatsServerConfig {
        name: LOCAL_CLUSTER_KEY.to_string(),
        url,
        token: Some(token),
        tls: None,
        tls_cert: None,
        tls_key: None,
        tls_ca: None,
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

    async fn resolve_nats_server<'a>(
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

        let options = build_nats_connect_options(server).with_context(|| {
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

fn build_nats_connect_options(server: &NatsServerConfig) -> Result<ConnectOptions> {
    let mut options = ConnectOptions::new();
    options = apply_nats_auth_options(options, server);
    options = apply_nats_tls_options(options, server)?;

    if should_require_tls(server) {
        options = options.require_tls(true);
    }

    Ok(options)
}

fn apply_nats_auth_options(
    mut options: ConnectOptions,
    server: &NatsServerConfig,
) -> ConnectOptions {
    if let Some(token) = &server.token {
        options = options.token(token.clone());
    }
    options
}

fn apply_nats_tls_options(
    mut options: ConnectOptions,
    server: &NatsServerConfig,
) -> Result<ConnectOptions> {
    reject_unsupported_tls_combo(server)?;
    options = apply_client_certificate_options(options, server)?;
    if let Some(tls_client) = build_custom_tls_client_config(server)? {
        options = options.tls_client_config(tls_client);
    }
    Ok(options)
}

fn reject_unsupported_tls_combo(server: &NatsServerConfig) -> Result<()> {
    // A custom `tls_ca` builds a dedicated rustls ClientConfig (below) which
    // replaces any client certificate registered via `add_client_certificate`.
    // async-nats 0.42 has no way to attach both a custom root store AND a client
    // cert through its high-level options, so reject the combination explicitly
    // rather than silently dropping the client certificate (which would make
    // mTLS fail at handshake time with a confusing error).
    if has_client_certificate(server) && server.tls_ca.is_some() {
        bail!(
            "NATS cluster '{}' sets both tls_ca and a client certificate (tls_cert/tls_key); \
             this combination is not supported (the custom CA would override the client cert). \
             Use a server cert chained to a publicly-trusted CA for mTLS, or drop tls_ca.",
            server.name
        );
    }
    Ok(())
}

fn apply_client_certificate_options(
    mut options: ConnectOptions,
    server: &NatsServerConfig,
) -> Result<ConnectOptions> {
    match (&server.tls_cert, &server.tls_key) {
        (Some(cert), Some(key)) => {
            let cert_path = validate_tls_path(server, "tls_cert", cert)?;
            let key_path = validate_tls_path(server, "tls_key", key)?;
            options = options.add_client_certificate(cert_path, key_path);
            Ok(options)
        }
        (Some(_), None) => bail!(
            "NATS cluster '{}' sets tls_cert but missing tls_key",
            server.name
        ),
        (None, Some(_)) => bail!(
            "NATS cluster '{}' sets tls_key but missing tls_cert",
            server.name
        ),
        (None, None) => Ok(options),
    }
}

fn build_custom_tls_client_config(
    server: &NatsServerConfig,
) -> Result<Option<async_nats::rustls::ClientConfig>> {
    let Some(ca_path) = &server.tls_ca else {
        return Ok(None);
    };

    let ca_path = validate_tls_path(server, "tls_ca", ca_path)?;
    let ca_file = std::fs::File::open(&ca_path).with_context(|| {
        format!(
            "Failed to read tls_ca '{}' for NATS cluster '{}'",
            ca_path.display(),
            server.name
        )
    })?;
    let certs = rustls_pemfile::certs(&mut BufReader::new(ca_file))
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "Failed to parse PEM certificates from tls_ca '{}' for NATS cluster '{}'",
                ca_path.display(),
                server.name
            )
        })?;
    let mut root_store = async_nats::rustls::RootCertStore::empty();
    let (added, ignored) = root_store.add_parsable_certificates(certs);
    if added == 0 {
        bail!(
            "Failed to parse tls_ca '{}' for NATS cluster '{}' (ignored {ignored} certs)",
            ca_path.display(),
            server.name
        );
    }
    let tls_client = async_nats::rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(Some(tls_client))
}

fn has_client_certificate(server: &NatsServerConfig) -> bool {
    server.tls_cert.is_some() || server.tls_key.is_some()
}

fn should_require_tls(server: &NatsServerConfig) -> bool {
    server.tls.unwrap_or(false) || has_client_certificate(server) || server.tls_ca.is_some()
}

fn validate_tls_path(server: &NatsServerConfig, field: &str, value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.exists() {
        bail!(
            "NATS cluster '{}' {} path '{}' does not exist",
            server.name,
            field,
            path.display()
        );
    }
    if !path.is_file() {
        bail!(
            "NATS cluster '{}' {} path '{}' is not a file",
            server.name,
            field,
            path.display()
        );
    }
    Ok(path)
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
    async fn local_cluster_env_handoff_builds_authenticated_dynamic_config() {
        harnx_core::require_nextest();
        unsafe {
            std::env::set_var(HARNX_NATS_URL_ENV, "nats://127.0.0.1:4555");
            std::env::set_var(HARNX_NATS_TOKEN_ENV, "handoff-token");
        }

        let server = resolve_local_nats_server_config().await.unwrap();

        assert_authenticated_local_server(&server, "nats://127.0.0.1:4555", "handoff-token");
    }

    #[tokio::test]
    async fn local_cluster_dynamic_resolution_wins_over_reserved_yaml_entry() {
        harnx_core::require_nextest();
        unsafe {
            std::env::set_var(HARNX_NATS_URL_ENV, "nats://127.0.0.1:4666");
            std::env::set_var(HARNX_NATS_TOKEN_ENV, "dynamic-token");
        }
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
        unsafe {
            std::env::set_var("NATS_TOKEN", "secret-token");
            std::env::set_var("NATS_CERT", "/tmp/client-cert.pem");
            std::env::set_var("NATS_KEY", "/tmp/client-key.pem");
            std::env::set_var("NATS_CA", "/tmp/ca.pem");
        }

        let mut server = NatsServerConfig {
            name: "local".into(),
            url: "nats://${NATS_TOKEN}@localhost:4222".into(),
            token: Some("${NATS_TOKEN}".into()),
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

    #[test]
    fn build_nats_connect_options_rejects_partial_client_cert_config() {
        let server = NatsServerConfig {
            name: "mtls".into(),
            url: "tls://localhost:4222".into(),
            token: None,
            tls: Some(true),
            tls_cert: Some("/tmp/client-cert.pem".into()),
            tls_key: None,
            tls_ca: None,
            agents: vec![],
        };

        let error = build_nats_connect_options(&server).unwrap_err().to_string();
        assert_error_mentions(&error, &["tls_cert", "tls_key", "mtls"]);
    }

    #[test]
    fn build_nats_connect_options_rejects_missing_tls_file() {
        let server = NatsServerConfig {
            name: "secure".into(),
            url: "tls://localhost:4222".into(),
            token: Some("token".into()),
            tls: Some(true),
            tls_cert: Some("/definitely/missing-cert.pem".into()),
            tls_key: Some("/definitely/missing-key.pem".into()),
            tls_ca: None,
            agents: vec![],
        };

        let error = build_nats_connect_options(&server).unwrap_err().to_string();
        assert_error_mentions(&error, &["does not exist", "tls_cert", "secure"]);
    }

    #[test]
    fn build_nats_connect_options_rejects_custom_ca_with_client_cert() {
        // async-nats 0.42 cannot attach both a custom root store and a client
        // cert; the combination must be rejected, not silently dropped.
        let server = NatsServerConfig {
            name: "alb".into(),
            url: "tls://localhost:4222".into(),
            token: None,
            tls: Some(true),
            tls_cert: Some("/tmp/client-cert.pem".into()),
            tls_key: Some("/tmp/client-key.pem".into()),
            tls_ca: Some("/tmp/ca.pem".into()),
            agents: vec![],
        };

        let error = build_nats_connect_options(&server).unwrap_err().to_string();
        assert_error_mentions(&error, &["tls_ca", "not supported", "alb"]);
    }
}
