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
    /// Serve mode, optionally specify address to bind
    #[clap(long, value_name = "ADDRESS", num_args = 0..=1, hide = true)]
    pub serve: Option<Option<String>>,
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
    /// Add MCP roots
    #[clap(long, value_name = "PATH", value_delimiter = ',', hide = true)]
    pub mcp_root: Vec<String>,
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
    use super::{Cli, Commands, InfoSubcommands};
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
}
