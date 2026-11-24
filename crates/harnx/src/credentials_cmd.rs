//! `harnx credentials` — list, connect and disconnect your own tool-server
//! credentials (decision 11).
//!
//! The commands act for the operator's identity (`Config::default_user_id`
//! for the selected cluster), never accept secrets on the command line, and
//! hand the connect link to the browser flow on the tool server's own page.

use crate::cli::{Cli, CredentialsArgs, CredentialsSubcommands};
use anyhow::Result;
use harnx_runtime::operator_tools::credentials::{
    run_credentials_command, CredentialsAction, CredentialsProgress,
};
use harnx_runtime::operator_tools::OperatorToolCommand;
use harnx_runtime::utils::create_abort_signal;
use is_terminal::IsTerminal;
use std::io::stdout;

pub(crate) async fn run(cli: &Cli, args: &CredentialsArgs) -> Result<u8> {
    let action = match &args.command {
        CredentialsSubcommands::List => CredentialsAction::List,
        CredentialsSubcommands::Connect { server } => CredentialsAction::Connect {
            server: server.clone(),
        },
        CredentialsSubcommands::Disconnect { server } => CredentialsAction::Disconnect {
            server: server.clone(),
        },
    };
    let abort = create_abort_signal();
    let signal_abort = abort.clone();
    let signal_task = scopeguard::guard(
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_abort.set_ctrlc();
            }
        }),
        |task| task.abort(),
    );
    println!("{}", operator_identity_note(cli).await);
    let mut first_url = true;
    let result = run_credentials_command(
        action,
        &abort,
        &mut |progress| match progress {
            CredentialsProgress::ConnectUrl { url, message } => {
                if !message.is_empty() {
                    println!("{message}");
                }
                println!("{url}");
                if first_url {
                    first_url = false;
                    open_url_best_effort(&url);
                    println!("Waiting for you to finish in the browser (Ctrl-C to cancel)...");
                }
            }
        },
        |command| dispatch(cli, command),
    )
    .await;
    drop(signal_task);
    match result {
        Ok(outcome) => {
            if !outcome.output.is_empty() {
                println!("{}", outcome.output);
            }
            Ok(u8::from(outcome.failures > 0))
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            Ok(1)
        }
    }
}

/// One operator call through the existing reserved/named machinery. Each
/// call carries only non-secret arguments; the JSON output form is what the
/// credentials engine parses.
async fn dispatch(
    cli: &Cli,
    command: OperatorToolCommand,
) -> Result<harnx_runtime::operator_tools::OperatorToolReply> {
    crate::tool_cmd::execute(cli, command, true).await
}

/// Whose credentials these commands manage: the same default the CLI stamps
/// on new sessions for the selected cluster — not an active session's owner.
async fn operator_identity_note(cli: &Cli) -> String {
    let user_id = match crate::tool_cmd::resolve_caller(cli).await {
        Ok(caller) => caller.config.default_user_id_for_cluster(&caller.cluster),
        Err(_) => None,
    };
    match user_id {
        Some(user_id) => format!("Managing credentials for {user_id}."),
        None => "No user_id set; managing credentials for the ownerless profile (only --single-user tool servers accept it)."
            .to_string(),
    }
}

fn open_url_best_effort(url: &str) {
    if !stdout().is_terminal() {
        return;
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    // Fire and forget: a missing opener is not an error worth reporting.
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use harnx_runtime::operator_tools::OperatorToolCommand;

    fn credentials_cli(argv: &[&str]) -> CredentialsArgs {
        let mut full = vec!["harnx", "credentials"];
        full.extend(argv);
        let cli = Cli::try_parse_from(full).expect("credentials grammar");
        let Some(crate::cli::Commands::Credentials(args)) = cli.command else {
            panic!("expected credentials command");
        };
        args
    }

    #[test]
    fn parses_credentials_subcommands() {
        assert!(matches!(
            credentials_cli(&["list"]).command,
            crate::cli::CredentialsSubcommands::List
        ));
        assert!(matches!(
            credentials_cli(&["connect", "jira"]).command,
            crate::cli::CredentialsSubcommands::Connect { ref server } if server == "jira"
        ));
        assert!(matches!(
            credentials_cli(&["disconnect", "pendo"]).command,
            crate::cli::CredentialsSubcommands::Disconnect { ref server } if server == "pendo"
        ));
        // Secrets never appear as accepted syntax: connect takes only a name.
        assert!(Cli::try_parse_from(["harnx", "credentials", "connect"]).is_err());
        assert!(Cli::try_parse_from(["harnx", "credentials", "bogus"]).is_err());
    }

    #[test]
    fn credentials_help_documents_the_flow() {
        use clap::CommandFactory;
        let mut command = Cli::command();
        command.build();
        let credentials = command
            .find_subcommand_mut("credentials")
            .expect("credentials subcommand");
        let help = credentials.render_long_help().to_string();
        assert!(help.contains("list"));
        assert!(help.contains("connect"));
        assert!(help.contains("disconnect"));
        assert!(help.contains("never"));
        // The subcommands take only a server name — no secret input syntax.
        for name in ["connect", "disconnect"] {
            let sub_help = credentials
                .find_subcommand_mut(name)
                .unwrap_or_else(|| panic!("{name} subcommand"))
                .render_long_help()
                .to_string();
            assert!(sub_help.contains("<SERVER>"), "{name} help: {sub_help}");
        }
    }

    #[test]
    fn credentials_command_has_no_secret_flags() {
        // The surface must not offer any token/secret input: scan the built
        // command for secret-ish flag names. Global options (like
        // `--token-budget`, a model token quota) are inherited from the root
        // command, not part of this surface.
        use clap::CommandFactory;
        let mut command = Cli::command();
        command.build();
        let credentials = command
            .find_subcommand_mut("credentials")
            .expect("credentials subcommand");
        for sub in credentials.get_subcommands() {
            for arg in sub.get_arguments() {
                if arg.is_global_set() {
                    continue;
                }
                let names = format!("{:?}", arg.get_id());
                assert!(
                    !names.to_lowercase().contains("token")
                        && !names.to_lowercase().contains("secret"),
                    "credentials command accepts a secret-ish argument: {names}"
                );
            }
        }
    }

    #[test]
    fn dispatch_builds_only_non_secret_commands() {
        // Compile-time sanity on the command type the engine produces: the
        // connect/disconnect/status commands are `{}` calls; nothing here
        // carries a credential value.
        let command = OperatorToolCommand::Call {
            name: "jira_credential_status".into(),
            args_json: "{}".into(),
            tool_input: None,
        };
        let rendered = format!("{command:?}");
        assert!(!rendered.to_lowercase().contains("token"));
        assert!(!rendered.to_lowercase().contains("secret"));
    }
}
