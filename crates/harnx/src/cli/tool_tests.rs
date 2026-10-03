//! Tool inspection/invocation grammar and global flag placement.
use super::*;

#[test]
fn tool_grammar_has_no_duplicate_clap_arguments() {
    use clap::CommandFactory;
    Cli::command().debug_assert();
}

#[test]
fn parses_info_tool_human_default_and_json() {
    for json in [false, true] {
        let mut argv = vec!["harnx", "info", "tool", "fs.inspect"];
        if json {
            argv.push("--json");
        }
        let cli = Cli::try_parse_from(argv).unwrap();
        assert_eq!(cli.agent, None);
        assert_eq!(
            cli.command,
            Some(Commands::Info(InfoArgs {
                command: InfoSubcommands::Tool(InfoToolArgs {
                    name: "fs.inspect".into(),
                    json,
                }),
            }))
        );
    }
}

#[test]
fn parses_list_tools_with_omitted_or_quoted_pattern() {
    for pattern in [None, Some("fs.*"), Some("tool name with spaces")] {
        for json in [false, true] {
            let mut argv = vec!["harnx", "list", "tools"];
            if let Some(pattern) = pattern {
                argv.push(pattern);
            }
            if json {
                argv.push("--json");
            }
            let cli = Cli::try_parse_from(argv).unwrap();
            assert_eq!(cli.agent, None);
            assert_eq!(
                cli.command,
                Some(Commands::List(ListArgs {
                    command: ListSubcommands::Tools(ListToolsArgs {
                        pattern: pattern.map(str::to_owned),
                        json,
                    }),
                }))
            );
        }
    }
}

#[test]
fn parses_call_tool_preserving_raw_json_quotes_spaces_and_newlines() {
    // Shell quoting produces one argv entry. The parser must not split or
    // reconstruct it; runtime validation belongs to the invocation path.
    for raw in [
        r#"{"command": "printf \"hello world\"", "nested": {"path": "a b"}}"#,
        "{\n  \"text\": \"two words\",\n  \"items\": [1, null]\n}",
        "{}",
        "not JSON yet",
    ] {
        for json in [false, true] {
            let mut argv = vec!["harnx", "call", "tool", "bash.exec", raw];
            if json {
                argv.push("--json");
            }
            let cli = Cli::try_parse_from(argv).unwrap();
            assert_eq!(cli.agent, None);
            assert_eq!(
                cli.command,
                Some(Commands::Call(CallArgs {
                    command: CallSubcommands::Tool(CallToolArgs {
                        name: "bash.exec".into(),
                        args_json: raw.into(),
                        json,
                    }),
                }))
            );
        }
    }
}

#[test]
fn tool_commands_inherit_agent_at_top_level_action_and_subject_positions() {
    for command in [
        vec!["info", "tool", "fs.inspect", "--json"],
        vec!["list", "tools", "--json"],
        vec!["list", "tools", "fs.*", "--json"],
        vec![
            "call",
            "tool",
            "bash.exec",
            r#"{"command": "echo two words"}"#,
            "--json",
        ],
    ] {
        let expected = Cli::try_parse_from(std::iter::once("harnx").chain(command.iter().copied()))
            .unwrap()
            .command;
        for flag in ["--agent", "-a"] {
            // Before action, between action/subject, after subject, and at end.
            for position in [0, 1, 2, command.len()] {
                let mut argv = vec!["harnx"];
                argv.extend_from_slice(&command[..position]);
                argv.extend([flag, "pantheon/atlas@prod"]);
                argv.extend_from_slice(&command[position..]);
                let cli = Cli::try_parse_from(argv).unwrap();
                assert_eq!(cli.agent.as_deref(), Some("pantheon/atlas@prod"));
                assert_eq!(cli.command, expected);
            }
        }
    }
}

#[test]
fn tool_commands_reject_invalid_subjects_missing_and_extra_arguments() {
    for argv in [
        vec!["harnx", "info", "tools", "fs.inspect"],
        vec!["harnx", "list", "tool"],
        vec!["harnx", "call", "tools", "fs.inspect", "{}"],
        vec!["harnx", "call", "agent", "atlas", "{}"],
        vec!["harnx", "info", "tool"],
        vec!["harnx", "call", "tool", "fs.inspect"],
        vec!["harnx", "list", "tools", "fs.*", "extra"],
        vec!["harnx", "info", "tool", "fs.inspect", "extra"],
        vec!["harnx", "call", "tool", "fs.inspect", "{}", "extra"],
        vec!["harnx", "info", "tool", "fs.inspect", "--json", "--json"],
    ] {
        assert!(
            Cli::try_parse_from(&argv).is_err(),
            "unexpectedly accepted {argv:?}"
        );
    }
}

#[test]
fn tool_json_flags_do_not_change_existing_subjects() {
    assert!(Cli::try_parse_from(["harnx", "list", "sessions", "--json"]).is_err());
    assert!(Cli::try_parse_from(["harnx", "info", "agent", "atlas", "--json"]).is_err());
    let cli = Cli::try_parse_from(["harnx", "prompt", "call", "tool", "fs.inspect", "{}"]).unwrap();
    let Some(Commands::Prompt(args)) = cli.command else {
        panic!("expected prompt")
    };
    assert_eq!(args.text, ["call", "tool", "fs.inspect", "{}"]);
}
