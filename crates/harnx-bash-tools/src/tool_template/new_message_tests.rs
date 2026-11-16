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

#[test]
fn template_protocol_defaults_to_none_and_parses_jsonl() {
    let source = Path::new("test.yaml");
    let plain = parse_template_str("name: t\nscript: echo\n", source).unwrap();
    assert_eq!(plain.protocol, None);
    let jsonl = parse_template_str("name: t\nscript: echo\nprotocol: jsonl\n", source).unwrap();
    assert_eq!(jsonl.protocol, Some(ToolProtocol::Jsonl));
}

#[test]
fn protocol_output_control_descriptions_mention_stderr() {
    let source = Path::new("test.yaml");
    let description = |yaml: &str| {
        let schema = parse_template_str(yaml, source)
            .unwrap()
            .input_schema()
            .unwrap();
        schema["properties"]["head_lines"]["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let protocol = description("name: t\nscript: echo\nprotocol: jsonl\n");
    assert!(protocol.contains("stderr"), "{protocol}");
    assert_eq!(
        description("name: t\nscript: echo\n"),
        output::HEAD_LINES_DESCRIPTION
    );
}

#[test]
fn template_protocol_rejects_unknown_value() {
    let error = parse_template_str(
        "name: t\nscript: echo\nprotocol: xml\n",
        Path::new("p.yaml"),
    )
    .unwrap_err();
    let message = format!("{error:#}");
    for expected in ["p.yaml", "protocol", "unknown variant `xml`", "`jsonl`"] {
        assert!(message.contains(expected), "{message}");
    }
}
