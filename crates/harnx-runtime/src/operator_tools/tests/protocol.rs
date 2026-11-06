use super::*;

#[test]
fn call_bash_tui_parse_preserves_raw_script_and_uses_session_selection() {
    // `.call bash` (#2630) consumes the raw remainder: quotes, double
    // spaces and hyphens stay script content verbatim.
    assert_eq!(
        parse_operator_line(".call bash echo \"spaced  out\" --flag").unwrap(),
        OperatorToolCommand::Bash {
            script: "echo \"spaced  out\" --flag".into(),
            selection: BashToolSelection::Session,
        }
    );
    assert_eq!(
        parse_operator_line(".call bash\tfs_read --path a | wc -l").unwrap(),
        OperatorToolCommand::Bash {
            script: "fs_read --path a | wc -l".into(),
            selection: BashToolSelection::Session,
        }
    );
    assert!(is_operator_tool_command(".call bash echo hi"));
    // A missing script is a usage error, not an empty script.
    let error = parse_operator_line(".call bash").unwrap_err().to_string();
    assert!(error.contains("Usage: .call bash <script>"), "{error}");
    // Existing `.call tool` grammar is unchanged and still validated.
    assert_eq!(
        parse_operator_line(".call tool visible {}").unwrap(),
        OperatorToolCommand::Call {
            name: "visible".into(),
            args_json: "{}".into()
        }
    );
    assert!(parse_operator_line(".call tool visible").is_err());
}

#[test]
fn legacy_command_forms_serialize_unchanged() {
    // Pins the pre-bash protocol shape (#2630): an older worker must keep
    // understanding these forms exactly as before.
    assert_eq!(
        serde_json::to_value(OperatorToolCommand::Info {
            name: "fs_read".into()
        })
        .unwrap(),
        json!({"action": "info", "name": "fs_read"})
    );
    assert_eq!(
        serde_json::to_value(OperatorToolCommand::List { pattern: None }).unwrap(),
        json!({"action": "list", "pattern": null})
    );
    assert_eq!(
        serde_json::to_value(OperatorToolCommand::List {
            pattern: Some("fs_*".into())
        })
        .unwrap(),
        json!({"action": "list", "pattern": "fs_*"})
    );
    assert_eq!(
        serde_json::to_value(OperatorToolCommand::Call {
            name: "fs_read".into(),
            args_json: "{\"a\": 1}".into()
        })
        .unwrap(),
        json!({"action": "call", "name": "fs_read", "args_json": "{\"a\": 1}"})
    );
}

#[test]
fn bash_command_round_trips_every_selection_mode() {
    let script = "fs_read '/tmp/a b' | fs_write \"out\"\n";
    for (selection, selection_json) in [
        (
            BashToolSelection::Inferred {
                names: vec!["fs_read".into(), "fs_write".into()],
            },
            json!({"mode": "inferred", "names": ["fs_read", "fs_write"]}),
        ),
        (
            BashToolSelection::Explicit {
                selectors: vec!["fs_{read_file,write_file}".into()],
            },
            json!({"mode": "explicit", "selectors": ["fs_{read_file,write_file}"]}),
        ),
        (BashToolSelection::Session, json!({"mode": "session"})),
    ] {
        let command = OperatorToolCommand::Bash {
            script: script.into(),
            selection,
        };
        let value = serde_json::to_value(&command).unwrap();
        assert_eq!(
            value,
            json!({"action": "bash", "script": script, "selection": selection_json})
        );
        // The raw script travels unchanged, including quotes and newlines.
        let round = serde_json::from_value::<OperatorToolCommand>(value).unwrap();
        assert_eq!(round, command);
        match round {
            OperatorToolCommand::Bash { script: raw, .. } => assert_eq!(raw, script),
            _ => unreachable!("round trip keeps the bash variant"),
        }
    }
}

#[test]
fn explicit_empty_selection_stays_distinct_from_inference() {
    // Explicit presence disables inference even when the selector list is
    // empty, so emptiness must survive serialization.
    let explicit = OperatorToolCommand::Bash {
        script: String::new(),
        selection: BashToolSelection::Explicit { selectors: vec![] },
    };
    let inferred = OperatorToolCommand::Bash {
        script: String::new(),
        selection: BashToolSelection::Inferred { names: vec![] },
    };
    assert_ne!(explicit, inferred);
    let value = serde_json::to_value(&explicit).unwrap();
    assert_eq!(
        value,
        json!({"action": "bash", "script": "", "selection": {"mode": "explicit", "selectors": []}})
    );
    assert_eq!(
        serde_json::from_value::<OperatorToolCommand>(value).unwrap(),
        explicit
    );
}

#[test]
fn legacy_worker_rejects_bash_command_with_clean_error() {
    // A worker built before #2630 deserializes the request enum itself, so
    // an unknown bash action must fail there with a clean serde error, not
    // hang or execute a wrong tool.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "action", rename_all = "snake_case")]
    #[allow(dead_code)] // deserialization target only; fields exist for shape
    enum LegacyCommand {
        Info { name: String },
        List { pattern: Option<String> },
        Call { name: String, args_json: String },
    }
    let value = json!({"action": "bash", "script": "echo hi", "selection": {"mode": "session"}});
    let error = serde_json::from_value::<LegacyCommand>(value)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown variant `bash`"), "actual: {error}");
    assert!(
        error.contains("expected one of `info`, `list`, `call`"),
        "actual: {error}"
    );
}
