use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use harnx_runtime::config::SessionFormat;
use is_terminal::IsTerminal;
use std::io::{stdin, Read};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
    /// Select a LLM model
    #[clap(short, long, global = true, hide = true)]
    pub model: Option<String>,
    /// Use the system prompt
    #[clap(long, global = true, hide = true)]
    pub prompt: Option<String>,
    /// Start or join a session
    #[clap(short = 's', long, global = true, hide = true)]
    pub session: Option<Option<String>>,
    /// Ensure the session is empty
    #[clap(long, global = true, hide = true)]
    pub empty_session: bool,
    /// Start a agent
    #[clap(short = 'a', long, global = true, hide = true)]
    pub agent: Option<String>,
    /// Set agent variable pairs (format: --agent-variable key value or -x key value); can be repeated
    #[clap(short = 'x', long, value_names = ["KEY", "VALUE"], num_args = 2, action = clap::ArgAction::Append, global = true, hide = true)]
    pub agent_variable: Vec<String>,
    /// Use the RAG
    #[clap(short = 'r', long, global = true, hide = true)]
    pub rag: Option<String>,
    /// Rebuild the RAG to sync document changes
    #[clap(long, global = true, hide = true)]
    pub rebuild_rag: bool,
    /// File to include with the message
    #[clap(short, long, value_name = "FILE", global = true, hide = true)]
    pub file: Vec<String>,
    /// Highlight code with provided theme
    #[clap(long, global = true, hide = true)]
    pub code_theme: Option<String>,
    /// Light theme for markdown rendering
    #[clap(long, global = true, hide = true)]
    pub light_theme: bool,
    /// Execute macro command(s) from config by name
    #[clap(long = "macro", value_name = "NAME", global = true, hide = true)]
    pub macro_name: Option<String>,
    /// Disable streaming output
    #[clap(long, global = true, hide = true)]
    pub no_stream: bool,
    /// Print only the final response in non-interactive mode
    #[clap(long, global = true, hide = true)]
    pub final_only: bool,
    /// Maximum one-shot invocation duration in seconds (0 or unset means no limit)
    #[clap(long, value_name = "SECONDS", global = true)]
    pub timeout_secs: Option<u64>,
    /// Maximum budgeted tokens for one-shot invocation (0 or unset means unlimited)
    #[clap(long, value_name = "TOKENS", global = true)]
    pub token_budget: Option<u64>,
    /// Abandon an unconfirmed cancellation before running a one-shot prompt
    #[clap(long, global = true)]
    pub resume_anyway: bool,
    /// Display the message without sending it
    #[clap(long, global = true, hide = true)]
    pub dry_run: bool,
    /// Display internal info
    #[clap(long, global = true, hide = true)]
    pub info: bool,
    /// Sync models updates
    #[clap(long, global = true, hide = true)]
    pub sync_models: bool,
    /// List all available chat models
    #[clap(long, global = true, hide = true)]
    pub list_models: bool,
    /// List all agents
    #[clap(long, global = true, hide = true)]
    pub list_agents: bool,
    /// List agents available for direct interaction (excludes subagent and compaction roles)
    #[clap(long, global = true, hide = true)]
    pub list_assistant_agents: bool,
    /// List all RAGs
    #[clap(long, global = true, hide = true)]
    pub list_rags: bool,
    /// List all macros
    #[clap(long, global = true, hide = true)]
    pub list_macros: bool,
    /// Enable tools or toolsets for this session (can be repeated, also accepts toolset names)
    #[clap(
        short = 't',
        long = "tool",
        value_name = "TOOL",
        global = true,
        hide = true
    )]
    pub tool: Vec<String>,
    /// Input text after an explicit `--` separator
    #[clap(last = true, hide = true)]
    text: Vec<String>,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Commands {
    /// Run a non-interactive prompt
    Prompt(PromptArgs),
    /// Inspect harnx state
    Info(InfoArgs),
    /// Dump session transcript (full history)
    Dump(DumpArgs),
    /// Delete resources
    Delete(DeleteArgs),
    /// List resources
    List(ListArgs),
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct PromptArgs {
    /// Input text
    #[arg(trailing_var_arg = true)]
    text: Vec<String>,
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct DeleteArgs {
    #[command(subcommand)]
    pub command: DeleteSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum DeleteSubcommands {
    /// Delete a remote NATS session log stream + lease key
    Session(DeleteSessionArgs),
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct DeleteSessionArgs {
    pub session_id: String,
    /// Cluster key from nats_servers/<name>.yaml
    #[arg(long)]
    pub cluster: String,
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct ListArgs {
    #[command(subcommand)]
    pub command: ListSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum ListSubcommands {
    /// List sessions (local or remote based on --agent context)
    Sessions,
}

#[derive(Args, Debug, PartialEq, Eq)]
pub struct DumpArgs {
    #[command(subcommand)]
    pub command: DumpSubcommands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum DumpSubcommands {
    /// Dump session transcript entries
    Session {
        agent_name: String,
        session_id: String,
        /// Output format: text (human-readable), yaml (--- documents), json (JSONL)
        #[arg(long, default_value = "text")]
        format: SessionFormat,
        /// Follow live updates (stream mode)
        #[arg(long)]
        follow: bool,
    },
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
    /// Print saved session metadata (no transcript)
    Session {
        agent_name: String,
        session_id: String,
        /// Output format: text (human-readable), yaml, json
        #[arg(long, default_value = "text")]
        format: SessionFormat,
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
        let text_args = match &self.command {
            Some(Commands::Prompt(args)) => &args.text,
            _ => &self.text,
        };
        if text_args.is_empty() {
            let trimmed = stdin_text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        } else {
            Ok(Some(text_args.join(" ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_info_agent(cli: Cli, expected: &str) {
        match cli.command {
            Some(Commands::Info(args)) => match args.command {
                InfoSubcommands::Agent { name } => {
                    assert_eq!(name, expected);
                }
                _ => panic!("unexpected info subcommand"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    fn assert_info_session(
        cli: Cli,
        expected_agent: &str,
        expected_session: &str,
        expected_format: SessionFormat,
    ) {
        match cli.command {
            Some(Commands::Info(args)) => match args.command {
                InfoSubcommands::Session {
                    agent_name,
                    session_id,
                    format,
                } => {
                    assert_eq!(agent_name, expected_agent);
                    assert_eq!(session_id, expected_session);
                    assert_eq!(format, expected_format);
                }
                _ => panic!("unexpected info subcommand"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_info_agent() {
        let cli = Cli::try_parse_from(["harnx", "info", "agent", "example-agent"]).unwrap();
        assert_info_agent(cli, "example-agent");
    }

    #[test]
    fn parses_info_session_default_format() {
        let cli =
            Cli::try_parse_from(["harnx", "info", "session", "my-agent", "sess-123"]).unwrap();
        assert_info_session(cli, "my-agent", "sess-123", SessionFormat::Text);
    }

    #[test]
    fn parses_info_session_yaml_format() {
        let cli = Cli::try_parse_from([
            "harnx", "info", "session", "my-agent", "sess-123", "--format", "yaml",
        ])
        .unwrap();
        assert_info_session(cli, "my-agent", "sess-123", SessionFormat::Yaml);
    }

    #[test]
    fn parses_dump_session_with_format_and_follow() {
        let cli = Cli::try_parse_from([
            "harnx", "dump", "session", "a", "id", "--format", "json", "--follow",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Dump(args)) => match args.command {
                DumpSubcommands::Session {
                    agent_name,
                    session_id,
                    format,
                    follow,
                } => {
                    assert_eq!(agent_name, "a");
                    assert_eq!(session_id, "id");
                    assert_eq!(format, SessionFormat::Json);
                    assert!(follow);
                }
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_delete_session() {
        let cli =
            Cli::try_parse_from(["harnx", "delete", "session", "sess-1", "--cluster", "local"])
                .unwrap();
        match cli.command {
            Some(Commands::Delete(args)) => match args.command {
                DeleteSubcommands::Session(delete) => {
                    assert_eq!(delete.session_id, "sess-1");
                    assert_eq!(delete.cluster, "local");
                }
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_list_sessions() {
        let cli = Cli::try_parse_from(["harnx", "list", "sessions"]).unwrap();
        match cli.command {
            Some(Commands::List(args)) => match args.command {
                ListSubcommands::Sessions => {}
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rejects_old_session_delete_syntax() {
        // Old syntax: harnx session delete <id> --cluster <cluster>
        let result =
            Cli::try_parse_from(["harnx", "session", "delete", "sess-1", "--cluster", "local"]);
        assert!(
            result.is_err(),
            "Old session delete syntax should be rejected"
        );
    }

    #[test]
    fn rejects_old_list_sessions_flag() {
        // Old syntax: harnx --list-sessions
        let result = Cli::try_parse_from(["harnx", "--list-sessions"]);
        assert!(
            result.is_err(),
            "Old --list-sessions flag should be rejected"
        );
    }

    #[test]
    fn rejects_format_with_short_f() {
        // `-f` should parse as --file, not --format
        let result = Cli::try_parse_from(["harnx", "dump", "session", "a", "id", "-f", "json"]);
        // Should either error or parse `json` as a file, not as format
        if let Ok(cli) = result {
            if let Some(Commands::Dump(args)) = cli.command {
                let DumpSubcommands::Session { format, .. } = args.command;
                // If it parses, -f was interpreted as --file, not --format
                // format should still be Text (default)
                assert_eq!(format, SessionFormat::Text);
                // And file should contain "json"
                assert!(cli.file.contains(&"json".to_string()));
            }
        }
        // Either it errors, or -f is treated as file (not format)
    }

    #[test]
    fn unrecognized_words_are_not_implicit_prompt_text() {
        assert!(Cli::try_parse_from(["harnx", "worker", "--cluster", "prod"]).is_err());
        assert!(Cli::try_parse_from(["harnx", "hello"]).is_err());
    }

    #[test]
    fn parses_prompt_subcommand_with_options_before_or_after_it() {
        let before = Cli::try_parse_from(["harnx", "--agent", "foo", "prompt", "hello"]).unwrap();
        let after = Cli::try_parse_from(["harnx", "prompt", "--agent", "foo", "hello"]).unwrap();

        for cli in [before, after] {
            assert_eq!(cli.agent.as_deref(), Some("foo"));
            assert_eq!(
                cli.command,
                Some(Commands::Prompt(super::PromptArgs {
                    text: vec!["hello".to_string()],
                }))
            );
        }
    }

    #[test]
    fn parses_resume_anyway_for_one_shot_prompt() {
        let cli = Cli::try_parse_from([
            "harnx",
            "--agent",
            "foo",
            "--session",
            "stuck",
            "prompt",
            "--resume-anyway",
            "continue",
        ])
        .unwrap();
        assert!(cli.resume_anyway);
        assert_eq!(cli.text().unwrap().as_deref(), Some("continue"));
    }

    #[test]
    fn parses_prompt_invocation_limits_before_or_after_subcommand() {
        let before = Cli::try_parse_from([
            "harnx",
            "--timeout-secs",
            "12",
            "--token-budget",
            "345",
            "prompt",
            "hello",
        ])
        .unwrap();
        let after = Cli::try_parse_from([
            "harnx",
            "prompt",
            "--timeout-secs",
            "12",
            "--token-budget",
            "345",
            "hello",
        ])
        .unwrap();

        for cli in [before, after] {
            assert_eq!(cli.timeout_secs, Some(12));
            assert_eq!(cli.token_budget, Some(345));
        }
    }

    #[test]
    fn prompt_help_exposes_invocation_limits() {
        use clap::CommandFactory;
        let mut command = Cli::command();
        command.build();
        let help = command
            .find_subcommand_mut("prompt")
            .expect("prompt subcommand")
            .render_long_help()
            .to_string();

        assert!(help.contains("--timeout-secs <SECONDS>"));
        assert!(help.contains("0 or unset means no limit"));
        assert!(help.contains("--token-budget <TOKENS>"));
        assert!(help.contains("0 or unset means unlimited"));
        assert!(help.contains("--resume-anyway"));
    }

    #[test]
    fn parses_explicit_separator_prompt() {
        let cli = Cli::try_parse_from(["harnx", "--agent", "foo", "--", "info"]).unwrap();

        assert!(cli.command.is_none());
        assert_eq!(cli.agent.as_deref(), Some("foo"));
        assert_eq!(cli.text, vec!["info"]);
    }

    #[test]
    fn prompt_subcommand_accepts_option_like_text_after_separator() {
        let cli = Cli::try_parse_from(["harnx", "prompt", "--", "review", "--staged"]).unwrap();

        assert_eq!(
            cli.command,
            Some(Commands::Prompt(super::PromptArgs {
                text: vec!["review".to_string(), "--staged".to_string()],
            }))
        );
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
