use super::*;

fn parse_defaults(output: &str) -> Result<crate::tool_template::ToolTemplate> {
    parse_template_str(
        &format!("name: defaults\nscript: echo ok\noutput: {output}\n"),
        Path::new("defaults.yaml"),
    )
}

#[test]
fn template_output_defaults_accept_non_negative_integers_and_publish_schema_defaults() {
    for value in [0, 65536] {
        let template = parse_defaults(&format!(
            "{{head_lines: {value}, tail_lines: {value}, max_output_bytes: {value}}}"
        ))
        .unwrap();
        let bound = template.validate_and_bind(&Map::new()).unwrap();
        let schema = template.input_schema().unwrap();
        assert!(bound.env.is_empty());
        assert_eq!(
            bound.output,
            TemplateOutputOptions {
                head_lines: Some(value),
                tail_lines: Some(value),
                max_output_bytes: Some(value),
            }
        );
        for (name, _) in OUTPUT_CONTROL_PARAMS {
            assert_eq!(schema["properties"][name]["default"], value);
        }
    }

    let template =
        parse_defaults("{head_lines: 4, tail_lines: 6, max_output_bytes: 65536}").unwrap();
    let schema = template.input_schema().unwrap();
    assert_eq!(schema["properties"]["head_lines"]["default"], 4);
    assert_eq!(schema["properties"]["tail_lines"]["default"], 6);
    assert_eq!(schema["properties"]["max_output_bytes"]["default"], 65536);
}

#[test]
fn template_output_defaults_reject_invalid_values_at_load_time() {
    for (name, _) in OUTPUT_CONTROL_PARAMS {
        for value in [
            "-1",
            "1.5",
            "'4'",
            "true",
            "[]",
            "{}",
            "null",
            "18446744073709551616",
        ] {
            let error = parse_defaults(&format!("{{{name}: {value}}}")).unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(name), "{message}");
            assert!(message.contains("output"), "{message}");
            assert!(message.contains("defaults.yaml"), "{message}");
        }
    }
}

#[test]
fn template_output_defaults_reject_unknown_keys_at_load_time() {
    for name in ["max_output_byte", "timeout_secs"] {
        let error = parse_defaults(&format!("{{{name}: 5}}")).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains(&format!("unknown field `{name}`")),
            "{message}"
        );
        assert!(message.contains("defaults.yaml"), "{message}");
    }
}

#[test]
fn template_output_defaults_are_overridden_per_field_including_zero() {
    let template =
        parse_defaults("{head_lines: 4, tail_lines: 6, max_output_bytes: 65536}").unwrap();
    for (name, _) in OUTPUT_CONTROL_PARAMS {
        for value in [0, 1] {
            let mut expected = json!({"head_lines": 4, "tail_lines": 6, "max_output_bytes": 65536});
            expected[name] = json!(value);
            let args = json!({name: value});
            let bound = template
                .validate_and_bind(args.as_object().unwrap())
                .unwrap();
            assert_eq!(
                bound.output,
                TemplateOutputOptions::from_args(expected.as_object().unwrap()).unwrap()
            );
            assert!(bound.env.is_empty());
        }
    }
    let null_args = json!({"head_lines": null, "tail_lines": null, "max_output_bytes": null});
    assert_eq!(
        template
            .validate_and_bind(null_args.as_object().unwrap())
            .unwrap()
            .output,
        template.output
    );
}

#[test]
fn template_without_output_defaults_keeps_existing_options_and_schema() {
    for yaml in [
        "name: empty\nscript: echo ok",
        "name: empty\nscript: echo ok\noutput: {}",
    ] {
        let template = parse_template_str(yaml, Path::new("empty.yaml")).unwrap();
        assert_eq!(
            template.validate_and_bind(&Map::new()).unwrap().output,
            TemplateOutputOptions::default()
        );
        let schema = template.input_schema().unwrap();
        for (name, _) in OUTPUT_CONTROL_PARAMS {
            assert!(schema["properties"][name].get("default").is_none());
        }
    }
}

#[test]
fn shipped_lookup_templates_load_with_default_byte_limits() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/pantheon");
    let templates = discover_templates(Some(&package), &[], &[]).unwrap();
    for name in ["gh_issue_view", "jira_view"] {
        let template = templates
            .iter()
            .find(|template| template.name == name)
            .unwrap();
        assert_eq!(
            template.output,
            TemplateOutputOptions {
                max_output_bytes: Some(65536),
                ..Default::default()
            }
        );
        assert_eq!(
            template.input_schema().unwrap()["properties"]["max_output_bytes"]["default"],
            65536
        );
    }
}
