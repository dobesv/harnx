use super::*;

#[test]
fn template_on_user_message_defaults_to_ignore_and_parses_actions() {
    let source = Path::new("test.yaml");
    for (policy, expected) in [
        ("", OnUserMessage::Ignore),
        ("on_user_message: ignore", OnUserMessage::Ignore),
        ("on_user_message: terminate", OnUserMessage::Terminate),
    ] {
        let yaml = format!("name: test_template\nscript: echo test\n{policy}\n");
        let template = parse_template_str(&yaml, source).unwrap();
        assert_eq!(template.on_user_message, expected);
    }
    for value in ["true", "false", "null", "1", "[]"] {
        let yaml = format!("name: bad\nscript: echo test\non_user_message: {value}");
        assert!(parse_template_str(&yaml, source).is_err(), "{yaml}");
    }
}

#[test]
fn template_on_user_message_rejects_unknown_action_with_expected_values() {
    let error = parse_template_str(
        "name: bad\nscript: echo test\non_user_message: pause",
        Path::new("unknown-action.yaml"),
    )
    .unwrap_err();
    let message = format!("{error:#}");
    for expected in [
        "unknown-action.yaml",
        "on_user_message",
        "unknown variant `pause`",
        "`ignore`",
        "`terminate`",
    ] {
        assert!(message.contains(expected), "{message}");
    }
}
