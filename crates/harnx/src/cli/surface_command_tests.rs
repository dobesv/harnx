//! Clap parsing tests for `harnx surface` subcommands.

use clap::Parser;

use crate::cli::{
    Cli, Commands, SurfaceActArgs, SurfaceArgs, SurfaceSessionArgs, SurfaceSubcommands,
};

#[test]
fn parses_surface_act_and_enforces_required_ids_and_data_exclusivity() {
    let cli = Cli::try_parse_from([
        "harnx",
        "surface",
        "act",
        "my-agent",
        "session-1",
        "--id",
        "deploy-1",
        "--publish-seq",
        "7",
        "--action",
        "ship",
        "--data",
        "name=rel-1",
        "--note",
        "deploy now",
    ])
    .unwrap();
    assert_eq!(
        cli.command,
        Some(Commands::Surface(SurfaceArgs {
            command: SurfaceSubcommands::Act(SurfaceActArgs {
                agent: "my-agent".into(),
                session: "session-1".into(),
                surface_id: "deploy-1".into(),
                publish_seq: Some(7),
                action: "ship".into(),
                data: vec!["name=rel-1".into()],
                data_json: None,
                note: Some("deploy now".into()),
            }),
        }))
    );

    assert!(
        Cli::try_parse_from(["harnx", "surface", "act", "a", "s", "--action", "ship",]).is_err()
    );
    assert!(
        Cli::try_parse_from(["harnx", "surface", "act", "a", "s", "--id", "surface-1",]).is_err()
    );

    assert!(
        Cli::try_parse_from([
            "harnx",
            "surface",
            "act",
            "a",
            "s",
            "--id",
            "surface-1",
            "--action",
            "ship",
            "--data",
            "name=rel-1",
            "--data-json",
            r#"{"name":"rel-1"}"#,
        ])
        .is_err(),
        "--data and --data-json must conflict"
    );
}

#[test]
fn surface_list_requires_agent_and_session() {
    assert!(Cli::try_parse_from(["harnx", "surface", "list"]).is_err());
    assert!(Cli::try_parse_from(["harnx", "surface", "list", "agent"]).is_err());
    let cli = Cli::try_parse_from(["harnx", "surface", "list", "agent", "session"]).unwrap();
    assert_eq!(
        cli.command,
        Some(Commands::Surface(SurfaceArgs {
            command: SurfaceSubcommands::List(SurfaceSessionArgs {
                agent: "agent".into(),
                session: "session".into(),
            }),
        }))
    );
}
