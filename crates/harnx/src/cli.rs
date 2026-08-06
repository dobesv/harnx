use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use is_terminal::IsTerminal;
use std::io::{stdin, Read};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = None,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
    /// Select a LLM model
    #[clap(short, long, hide = true)]
    pub model: Option<String>,
    /// Use the system prompt
    #[clap(long, hide = true)]
    pub prompt: Option<String>,
    /// Start or join a session
    #[clap(short = 's', long, hide = true)]
    pub session: Option<Option<String>>,
    /// Ensure the session is empty
    #[clap(long, hide = true)]
    pub empty_session: bool,
    /// Ensure the new conversation is saved to the session
    #[clap(long, hide = true)]
    pub save_session: bool,
    /// Start a agent
    #[clap(short = 'a', long, hide = true)]
    pub agent: Option<String>,
    /// Set agent variable pairs (format: --agent-variable key value or -x key value); can be repeated
    #[clap(short = 'x', long, value_names = ["KEY", "VALUE"], num_args = 2, action = clap::ArgAction::Append, hide = true)]
    pub agent_variable: Vec<String>,
    /// Use the RAG
    #[clap(short = 'r', long, hide = true)]
    pub rag: Option<String>,
    /// Rebuild the RAG to sync document changes
    #[clap(long, hide = true)]
    pub rebuild_rag: bool,
    /// File to include with the message
    #[clap(short, long, value_name = "FILE", hide = true)]
    pub file: Vec<String>,
    /// Highlight code with provided theme
    #[clap(long, hide = true)]
    pub code_theme: Option<String>,
    /// Light theme for markdown rendering
    #[clap(long, hide = true)]
    pub light_theme: bool,
    /// Execute macro command(s) from config by name
    #[clap(long = "macro", value_name = "NAME", hide = true)]
    pub macro_name: Option<String>,
    /// Disable streaming output
    #[clap(long, hide = true)]
    pub no_stream: bool,
    /// Display the message without sending it
    #[clap(long, hide = true)]
    pub dry_run: bool,
    /// Display internal info
    #[clap(long, hide = true)]
    pub info: bool,
    /// Sync models updates
    #[clap(long, hide = true)]
    pub sync_models: bool,
    /// List all available chat models
    #[clap(long, hide = true)]
    pub list_models: bool,
    /// List all sessions
    #[clap(long, hide = true)]
    pub list_sessions: bool,
    /// List all agents
    #[clap(long, hide = true)]
    pub list_agents: bool,
    /// List agents available for direct interaction (excludes subagent and compaction roles)
    #[clap(long, hide = true)]
    pub list_assistant_agents: bool,
    /// List all RAGs
    #[clap(long, hide = true)]
    pub list_rags: bool,
    /// List all macros
    #[clap(long, hide = true)]
    pub list_macros: bool,
    /// Enable tools or toolsets for this session (can be repeated, also accepts toolset names)
    #[clap(short = 't', long = "tool", value_name = "TOOL", hide = true)]
    pub tool: Vec<String>,
    /// Input text
    #[clap(trailing_var_arg = true, hide = true)]
    text: Vec<String>,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Commands {
    /// Inspect harnx state
    Info(InfoArgs),
    /// Session management commands
    Session(SessionArgs),
    /// Run a worker daemon for a configured or shared-local NATS cluster
    Worker(WorkerArgs),
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct WorkerArgs {
    /// Cluster key from nats_servers/<name>.yaml, or __local__ with
    /// HARNX_NATS_URL and HARNX_NATS_TOKEN handoff
    #[arg(long)]
    pub cluster: String,
    /// Stable worker identity for leases and the durable consumer name.
    /// Defaults to a generated id if omitted.
    #[arg(long)]
    pub worker_id: Option<String>,
    /// Set agent variable pairs (format: --agent-variable key value or -x key value); can be repeated
    #[arg(short = 'x', long, value_names = ["KEY", "VALUE"], num_args = 2, action = clap::ArgAction::Append)]
    pub agent_variable: Vec<String>,
    /// Start this worker's tool servers, report which ones registered, and
    /// exit without serving sessions.
    #[arg(long)]
    pub diagnose: bool,
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum SessionSubcommands {
    /// Delete remote NATS session log stream + lease key for a cluster
    Delete(DeleteSessionArgs),
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct DeleteSessionArgs {
    pub session_id: String,
    /// Cluster key from nats_servers/<name>.yaml
    #[arg(long)]
    pub cluster: String,
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct InfoArgs {
    #[command(subcommand)]
    pub command: InfoSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum InfoSubcommands {
    /// Print fully-rendered agent markdown
    Agent { name: String },
    /// Print saved session state
    Session {
        agent_name: String,
        session_id: String,
    },
}

impl Cli {
    pub fn text(&self) -> Result<Option<String>> {
        let mut stdin_text = String::new();
        if !stdin().is_terminal() {
            let _ = stdin()
                .read_to_string(&mut stdin_text)
                .context("Invalid stdin pipe")?;
        };
        match self.text.is_empty() {
            true => {
                if stdin_text.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(stdin_text))
                }
            }
            false => {
                if self.macro_name.is_some() {
                    let text = self
                        .text
                        .iter()
                        .map(|v| shell_words::quote(v))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if stdin_text.is_empty() {
                        Ok(Some(text))
                    } else {
                        Ok(Some(format!("{text} -- {stdin_text}")))
                    }
                } else {
                    let text = self.text.join(" ");
                    if stdin_text.is_empty() {
                        Ok(Some(text))
                    } else {
                        Ok(Some(format!("{text}\n{stdin_text}")))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands, InfoSubcommands, SessionSubcommands, WorkerArgs};
    use clap::Parser;

    #[test]
    fn parses_info_agent_subcommand() {
        let cli = Cli::try_parse_from(["harnx", "info", "agent", "foo"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Info(super::InfoArgs {
                command: InfoSubcommands::Agent {
                    name: "foo".to_string(),
                },
            }))
        );
    }

    #[test]
    fn parses_legacy_flat_flag() {
        let cli = Cli::try_parse_from(["harnx", "--list-agents"]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.list_agents);
    }

    #[test]
    fn parses_session_delete_subcommand() {
        let cli =
            Cli::try_parse_from(["harnx", "session", "delete", "sess-1", "--cluster", "local"])
                .unwrap();
        match cli.command {
            Some(Commands::Session(args)) => match args.command {
                SessionSubcommands::Delete(delete) => {
                    assert_eq!(delete.session_id, "sess-1");
                    assert_eq!(delete.cluster, "local");
                }
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_worker_agent_variables() {
        let cli = Cli::try_parse_from([
            "harnx",
            "worker",
            "--cluster",
            "prod",
            "--agent-variable",
            "cloud_env",
            "true",
            "--agent-variable",
            "debug",
            "false",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Worker(WorkerArgs {
                cluster,
                worker_id,
                agent_variable,
                diagnose,
            })) => {
                assert_eq!(cluster, "prod");
                assert_eq!(worker_id, None);
                assert_eq!(agent_variable, vec!["cloud_env", "true", "debug", "false"]);
                assert!(!diagnose);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}

#[cfg(test)]
mod agent_ref_tests {
    use super::Cli;
    use clap::Parser;
    use harnx_core::agent_ref::AgentRef;

    #[test]
    fn cli_agent_flag_preserves_local_agent_refs() {
        let cli = Cli::try_parse_from(["harnx", "--agent", "pkg/bar"]).unwrap();
        assert_eq!(
            AgentRef::parse(cli.agent.as_deref().unwrap()),
            AgentRef::Local("pkg/bar".into())
        );
    }

    #[test]
    fn cli_agent_flag_preserves_remote_agent_refs() {
        let cli = Cli::try_parse_from(["harnx", "--agent", "bar@foo"]).unwrap();
        assert_eq!(
            AgentRef::parse(cli.agent.as_deref().unwrap()),
            AgentRef::Remote {
                agent: "bar".into(),
                cluster: "foo".into(),
            }
        );
    }
}
