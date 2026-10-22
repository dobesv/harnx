use super::*;
use clap::error::ErrorKind;

fn parse(extra: &[&str]) -> std::result::Result<DumpArgs, clap::Error> {
    let cli = Cli::try_parse_from(
        ["harnx", "dump", "session", "plain", "history"]
            .into_iter()
            .chain(extra.iter().copied()),
    )?;
    let Some(Commands::Dump(args)) = cli.command else {
        panic!("expected dump command");
    };
    Ok(args)
}

#[test]
fn parses_before_compaction_sequence_and_quoted_relative_count() {
    for value in ["15", "1 back", "2 back"] {
        let DumpSubcommands::Session(args) =
            parse(&["--before-compaction", value]).unwrap().command
        else {
            panic!("expected session");
        };
        assert_eq!(args.before_compaction, Some(value.parse().unwrap()));
        assert_eq!(args.format, SessionFormat::Text);
    }
}

#[test]
fn before_compaction_conflicts_with_live_and_loop_modes() {
    for flag in ["--follow", "--check-loop-detection"] {
        let error = parse(&[flag, "--before-compaction", "15"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
        assert!(error.to_string().contains(flag));
        assert!(error.to_string().contains("--before-compaction"));
    }
}

#[test]
fn before_compaction_rejects_malformed_values_during_parsing() {
    for value in ["0", "0 back", "many", "1 back extra"] {
        let error = parse(&["--before-compaction", value]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert!(error
            .to_string()
            .contains("compaction sequence or quoted \"N back\""));
    }
}

#[test]
fn before_compaction_requires_one_value() {
    for extra in [
        &["--before-compaction"][..],
        &["--before-compaction", "1", "back"],
        &["--before-compaction", "5", "--before-compaction", "15"],
    ] {
        assert!(parse(extra).is_err(), "{extra:?}");
    }
}

#[test]
fn before_compaction_rejects_structured_formats() {
    for format in ["json", "yaml"] {
        let DumpSubcommands::Session(args) =
            parse(&["--before-compaction", "15", "--format", format])
                .unwrap()
                .command
        else {
            panic!("expected session");
        };
        let error = args.validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "--before-compaction supports only --format text; JSON/YAML export the full log"
        );
    }
}

#[test]
fn before_compaction_accepts_explicit_text_format() {
    let DumpSubcommands::Session(args) = parse(&["--before-compaction", "15", "--format", "text"])
        .unwrap()
        .command
    else {
        panic!("expected session");
    };
    args.validate().unwrap();
}
