//! NATS cluster config loading and connection helpers.
use super::*;
use anyhow::{bail, Context, Result};
use async_nats::{jetstream, ConnectOptions};
use serde::{Deserialize, Serialize};
use std::{
    io::BufReader,
    path::{Path, PathBuf},
};

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
}

impl NatsServerConfig {
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = name.into();
    }
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

    pub async fn nats_client(&self, cluster_key: &str) -> Result<async_nats::Client> {
        let server = self.nats_server(cluster_key)?;
        Self::connect_nats_server(server).await.with_context(|| {
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
        };
        Config::expand_nats_server_envs(&mut server);

        assert_eq!(server.url, "nats://secret-token@localhost:4222");
        assert_eq!(server.token.as_deref(), Some("secret-token"));
        assert_eq!(server.tls_cert.as_deref(), Some("/tmp/client-cert.pem"));
        assert_eq!(server.tls_key.as_deref(), Some("/tmp/client-key.pem"));
        assert_eq!(server.tls_ca.as_deref(), Some("/tmp/ca.pem"));
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
        };

        let error = build_nats_connect_options(&server).unwrap_err().to_string();
        assert!(error.contains("tls_cert"));
        assert!(error.contains("tls_key"));
        assert!(error.contains("mtls"));
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
        };

        let error = build_nats_connect_options(&server).unwrap_err().to_string();
        assert!(error.contains("does not exist"));
        assert!(error.contains("tls_cert"));
        assert!(error.contains("secure"));
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
        };

        let error = build_nats_connect_options(&server).unwrap_err().to_string();
        assert!(error.contains("tls_ca"));
        assert!(error.contains("not supported"));
        assert!(error.contains("alb"));
    }
}
