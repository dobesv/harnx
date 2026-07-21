use std::fs;

use anyhow::{Context, Result, anyhow};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};

use crate::cli::Cli;

fn config_from_cli(cli: &Cli) -> Result<StreamableHttpClientTransportConfig> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(cli.url.as_str());
    config.auth_header = cli.bearer_token.clone();

    for header in &cli.header {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid header '{header}': expected NAME:VALUE"))?;
        let header_name: HeaderName = name.trim().parse()?;
        let header_value = HeaderValue::from_str(value.trim())?;
        config.custom_headers.insert(header_name, header_value);
    }

    if cli.strict_session {
        config.allow_stateless = false;
    }

    Ok(config)
}

pub fn build_transport(
    cli: &Cli,
) -> Result<StreamableHttpClientTransport<reqwest::Client>> {
    let mut client_builder = reqwest::Client::builder();

    match (&cli.tls_cert, &cli.tls_key) {
        (Some(cert_path), Some(key_path)) => {
            let cert = fs::read(cert_path)
                .with_context(|| format!("failed to read TLS cert PEM from {}", cert_path.display()))?;
            let key = fs::read(key_path)
                .with_context(|| format!("failed to read TLS key PEM from {}", key_path.display()))?;
            let mut identity_pem = cert;
            identity_pem.extend_from_slice(&key);
            let identity = reqwest::Identity::from_pem(&identity_pem)?;
            client_builder = client_builder.identity(identity);
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(anyhow!("--tls-cert and --tls-key must be provided together"));
        }
        (None, None) => {}
    }

    if let Some(ca_path) = &cli.tls_ca {
        let ca = fs::read(ca_path)
            .with_context(|| format!("failed to read TLS CA PEM from {}", ca_path.display()))?;
        let certificate = reqwest::Certificate::from_pem(&ca)?;
        client_builder = client_builder.add_root_certificate(certificate);
    }

    let client = client_builder.build()?;
    let config = config_from_cli(cli)?;

    Ok(StreamableHttpClientTransport::with_client(client, config))
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use reqwest::header::{HeaderName, HeaderValue};
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

    use crate::cli::Cli;

    fn config_from_args(args: &[&str]) -> StreamableHttpClientTransportConfig {
        let cli = Cli::parse_from(args);
        super::config_from_cli(&cli).expect("valid transport config")
    }

    #[test]
    fn bearer_token_sets_auth_header() {
        let config = config_from_args(&["harnx-mcp-remote", "--url", "https://example.com", "--bearer-token", "mytoken"]);
        assert_eq!(config.auth_header.as_deref(), Some("mytoken"));
    }

    #[test]
    fn custom_header_populates_custom_headers() {
        let config = config_from_args(&["harnx-mcp-remote", "--url", "https://example.com", "--header", "Foo:bar"]);
        assert_eq!(
            config
                .custom_headers
                .get(&"foo".parse::<HeaderName>().expect("header name")),
            Some(&HeaderValue::from_static("bar"))
        );
    }

    #[test]
    fn stateless_allowed_by_default() {
        let config = config_from_args(&["harnx-mcp-remote", "--url", "https://example.com"]);
        assert!(config.allow_stateless);
    }

    #[test]
    fn strict_session_disables_stateless_mode() {
        let config = config_from_args(&["harnx-mcp-remote", "--url", "https://example.com", "--strict-session"]);
        assert!(!config.allow_stateless);
    }

    #[test]
    fn missing_url_is_parse_error() {
        let err = Cli::try_parse_from(["harnx-mcp-remote"]).expect_err("missing url should fail");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}
