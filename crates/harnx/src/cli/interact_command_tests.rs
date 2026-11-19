//! Clap parsing tests for `harnx interact` subcommands.

use clap::Parser;

use crate::cli::{Cli, Commands, InteractArgs, InteractSessionArgs, InteractSubcommands};

#[test]
fn parses_interact_list() {
    let cli = Cli::try_parse_from(["harnx", "interact", "list", "my-agent@cluster", "session-1"])
        .unwrap();
    assert_eq!(
        cli.command,
        Some(Commands::Interact(InteractArgs {
            command: InteractSubcommands::List(InteractSessionArgs {
                agent: "my-agent@cluster".into(),
                session: "session-1".into(),
            }),
        }))
    );
}

#[test]
fn parses_interact_answer_with_data_and_note() {
    let cli = Cli::try_parse_from([
        "harnx",
        "interact",
        "answer",
        "my-agent",
        "session-1",
        "--id",
        "int-form",
        "--request-seq",
        "7",
        "--action",
        "deploy",
        "--data",
        "choice=Staging,Production",
        "--data",
        "/name=rel-1",
        "--note",
        "ship it",
    ])
    .unwrap();
    let Some(Commands::Interact(InteractArgs {
        command: InteractSubcommands::Answer(answer),
    })) = cli.command
    else {
        panic!("expected interact answer");
    };
    assert_eq!(answer.agent, "my-agent");
    assert_eq!(answer.session, "session-1");
    assert_eq!(answer.interaction_id, "int-form");
    assert_eq!(answer.request_seq, Some(7));
    assert_eq!(answer.action, "deploy");
    assert_eq!(
        answer.data,
        vec![
            "choice=Staging,Production".to_string(),
            "/name=rel-1".to_string()
        ]
    );
    assert_eq!(answer.data_json, None);
    assert_eq!(answer.note.as_deref(), Some("ship it"));
}

#[test]
fn parses_interact_answer_data_json_conflicts_with_data() {
    let cli = Cli::try_parse_from([
        "harnx",
        "interact",
        "answer",
        "a",
        "s",
        "--id",
        "i",
        "--action",
        "deploy",
        "--data-json",
        r#"{"choice": ["prod"]}"#,
    ])
    .unwrap();
    let Some(Commands::Interact(InteractArgs {
        command: InteractSubcommands::Answer(answer),
    })) = cli.command
    else {
        panic!("expected interact answer");
    };
    assert_eq!(answer.data_json.as_deref(), Some(r#"{"choice": ["prod"]}"#));
    // --data and --data-json are mutually exclusive.
    let both = Cli::try_parse_from([
        "harnx",
        "interact",
        "answer",
        "a",
        "s",
        "--id",
        "i",
        "--action",
        "deploy",
        "--data",
        "name=rel-1",
        "--data-json",
        r#"{"choice": ["prod"]}"#,
    ]);
    assert!(both.is_err(), "--data and --data-json must conflict");
}

#[test]
fn interact_answer_requires_action_and_id() {
    let missing_action =
        Cli::try_parse_from(["harnx", "interact", "answer", "a", "s", "--id", "i"]);
    assert!(missing_action.is_err(), "--action is required");
    let missing_id =
        Cli::try_parse_from(["harnx", "interact", "answer", "a", "s", "--action", "deny"]);
    assert!(missing_id.is_err(), "--id is required");
}

#[test]
fn interact_help_works() {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    cmd.build();
    let interact_cmd = cmd
        .find_subcommand_mut("interact")
        .expect("interact subcommand");
    let help = interact_cmd.render_long_help().to_string();
    assert!(help.contains("pending interactions"));
    let answer_cmd = interact_cmd
        .find_subcommand_mut("answer")
        .expect("interact answer subcommand");
    let answer_help = answer_cmd.render_long_help().to_string();
    assert!(answer_help.contains("--action"));
    assert!(answer_help.contains("--data-json"));
}
