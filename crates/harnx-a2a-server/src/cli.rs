//! Command-line options and unresolved agent export specifications.

use clap::Parser;
use std::{path::PathBuf, str::FromStr};

use crate::DEFAULT_A2A_HTTP_PORT;

/// An explicit agent export, resolved against configuration at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSpec {
    pub alias: Option<String>,
    pub name: String,
}

impl FromStr for AgentSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (alias, name) = match value.split_once('=') {
            Some((alias, name)) => {
                if alias.trim().is_empty() {
                    return Err(
                        "agent alias must not be empty (expected name or alias=name)".into(),
                    );
                }
                (Some(alias.trim().to_owned()), name.trim())
            }
            None => (None, value),
        };
        if name.is_empty() || name.contains('=') {
            return Err("agent name must be non-empty (expected name or alias=name)".into());
        }
        Ok(Self {
            alias,
            name: name.to_owned(),
        })
    }
}

/// Serve explicitly selected harnx agents over A2A JSON-RPC and SSE.
#[derive(Debug, Parser)]
#[command(name = "harnx-a2a-server", version, about, propagate_version = true)]
pub struct Args {
    /// HTTP bind host (set explicitly to expose externally).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// HTTP bind port.
    #[arg(long, default_value_t = DEFAULT_A2A_HTTP_PORT)]
    pub port: u16,

    /// Target cluster name for shared workers.
    #[arg(long)]
    pub cluster: Option<String>,

    /// Configuration directory path.
    #[arg(long, value_name = "PATH", env = "HARNX_CONFIG_DIR")]
    pub config_dir: Option<PathBuf>,

    /// Agent to export: name or alias=name (repeatable, comma-separated).
    /// CLI values replace HARNX_A2A_AGENTS; no default expose-all mode.
    #[arg(
        long = "agent",
        value_name = "SPEC",
        env = "HARNX_A2A_AGENTS",
        value_delimiter = ',',
        required = true
    )]
    pub agents: Vec<AgentSpec>,

    /// Public base URL for Agent Cards (otherwise inferred from proxy/Host headers).
    #[arg(long, value_name = "URL")]
    pub public_base_url: Option<String>,

    /// Trusted User-ID source: NAME, header:NAME or cookie:NAME (repeatable, first match wins;
    /// enables user isolation). Cookies must carry a proxy-verified user ID, not a token.
    #[arg(long, value_name = "NAME")]
    pub user_id_header: Vec<String>,

    /// Maximum rendered data/inline file part bytes per message.
    #[arg(long, default_value_t = 65536)]
    pub max_data_part_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{error::ErrorKind, CommandFactory};
    use std::ffi::OsString;

    struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

    impl EnvGuard {
        fn isolated() -> Self {
            harnx_core::require_nextest();
            let saved = ["HARNX_A2A_AGENTS", "HARNX_CONFIG_DIR"]
                .into_iter()
                .map(|key| {
                    let value = std::env::var_os(key);
                    std::env::remove_var(key);
                    (key, value)
                })
                .collect();
            Self(saved)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn cli_defaults_and_repeatable_alias_specs() {
        let _env = EnvGuard::isolated();
        let args = Args::try_parse_from([
            "harnx-a2a-server",
            "--agent",
            "foo",
            "--agent",
            "tools=pkg/agent",
        ])
        .unwrap();
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.port, 3020);
        assert_eq!(args.max_data_part_bytes, 65536);
        assert_eq!(args.cluster, None);
        assert_eq!(args.config_dir, None);
        assert_eq!(args.public_base_url, None);
        assert!(args.user_id_header.is_empty());
        assert_eq!(
            args.agents,
            vec![
                AgentSpec {
                    alias: None,
                    name: "foo".into()
                },
                AgentSpec {
                    alias: Some("tools".into()),
                    name: "pkg/agent".into()
                },
            ]
        );
    }

    #[test]
    fn cli_env_comma_list_and_config_dir() {
        let _env = EnvGuard::isolated();
        std::env::set_var("HARNX_A2A_AGENTS", "foo, tools=pkg/agent");
        std::env::set_var("HARNX_CONFIG_DIR", "config/a2a");
        let args = Args::try_parse_from(["harnx-a2a-server"]).unwrap();
        assert_eq!(args.agents.len(), 2);
        assert_eq!(args.agents[0].name, "foo");
        assert_eq!(args.agents[1].alias.as_deref(), Some("tools"));
        assert_eq!(args.agents[1].name, "pkg/agent");
        assert_eq!(args.config_dir, Some(PathBuf::from("config/a2a")));
    }

    #[test]
    fn cli_values_replace_env() {
        let _env = EnvGuard::isolated();
        std::env::set_var("HARNX_A2A_AGENTS", "foo,tools=pkg/agent");
        std::env::set_var("HARNX_CONFIG_DIR", "env-config");
        let args = Args::try_parse_from([
            "harnx-a2a-server",
            "--agent",
            "explicit",
            "--config-dir",
            "cli-config",
        ])
        .unwrap();
        assert_eq!(args.agents.len(), 1);
        assert_eq!(args.agents[0].name, "explicit");
        assert_eq!(args.config_dir, Some(PathBuf::from("cli-config")));
    }

    #[test]
    fn cli_missing_agent_error() {
        let _env = EnvGuard::isolated();
        let error = Args::try_parse_from(["harnx-a2a-server"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        assert!(error.to_string().contains("--agent <SPEC>"));
    }

    #[test]
    fn cli_invalid_agent_specs() {
        let _env = EnvGuard::isolated();
        for spec in ["", " ", "=name", "alias=", "alias=name=other"] {
            let error = Args::try_parse_from(["harnx-a2a-server", "--agent", spec]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "{spec:?}");
        }
    }

    #[test]
    fn cli_empty_env_list_rejected() {
        let _env = EnvGuard::isolated();
        for value in ["", "foo,", ",foo", "foo,,bar"] {
            std::env::set_var("HARNX_A2A_AGENTS", value);
            assert!(
                Args::try_parse_from(["harnx-a2a-server"]).is_err(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn cli_all_options_and_header_order() {
        let _env = EnvGuard::isolated();
        let args = Args::try_parse_from([
            "harnx-a2a-server",
            "--agent",
            "pkg/agent",
            "--host",
            "::1",
            "--port",
            "4020",
            "--cluster",
            "shared",
            "--config-dir",
            "config",
            "--public-base-url",
            "https://agents.example.com",
            "--user-id-header",
            "X-User-Id",
            "--user-id-header",
            "X-Forwarded-User",
            "--max-data-part-bytes",
            "1024",
        ])
        .unwrap();
        assert_eq!(args.host, "::1");
        assert_eq!(args.port, 4020);
        assert_eq!(args.cluster.as_deref(), Some("shared"));
        assert_eq!(args.config_dir, Some(PathBuf::from("config")));
        assert_eq!(
            args.public_base_url.as_deref(),
            Some("https://agents.example.com")
        );
        assert_eq!(args.user_id_header, ["X-User-Id", "X-Forwarded-User"]);
        assert_eq!(args.max_data_part_bytes, 1024);
    }

    #[test]
    fn cli_help_and_definition() {
        let _env = EnvGuard::isolated();
        Args::command().debug_assert();
        let error = Args::try_parse_from(["harnx-a2a-server", "--help"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        let help = error.to_string();
        for flag in [
            "--host",
            "--port",
            "--cluster",
            "--config-dir",
            "--agent",
            "--public-base-url",
            "--user-id-header",
            "--max-data-part-bytes",
        ] {
            assert!(help.contains(flag), "missing {flag}");
        }
        assert!(!help.contains("retention"));
    }
}
