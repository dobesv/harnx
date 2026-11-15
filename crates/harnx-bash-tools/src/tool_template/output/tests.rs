mod defaults;

use super::*;
use crate::server::ExecCommandParams;
use crate::tool_template::{discover_templates, parse_template_str};
use serde_json::json;
use std::path::Path;

#[test]
fn command_templates_expose_reserved_output_controls_with_exec_descriptions() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/pantheon");
    let mut templates = discover_templates(Some(&package), &[], &[]).unwrap();
    assert!(templates
        .iter()
        .any(|template| template.name == "gh_issue_view"));
    templates
        .push(parse_template_str("name: empty\nscript: echo ok", Path::new("empty.yaml")).unwrap());
    let exec_schema = serde_json::to_value(rmcp::schemars::schema_for!(ExecCommandParams)).unwrap();
    for template in templates {
        let schema = template.input_schema().unwrap();
        assert_eq!(schema["additionalProperties"], false);
        for name in [
            "head_lines",
            "tail_lines",
            "max_output_bytes",
            "timeout_secs",
        ] {
            let control = &schema["properties"][name];
            assert_eq!(control["type"], "integer", "{}: {name}", template.name);
            assert_eq!(
                control["description"],
                exec_schema["properties"][name]["description"]
            );
            assert!(!schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(name)));
        }
        for (name, _) in OUTPUT_CONTROL_PARAMS {
            assert_eq!(schema["properties"][name]["minimum"], 0);
        }
    }
}

#[test]
fn command_templates_reject_output_control_parameter_collisions() {
    for (name, _) in OUTPUT_CONTROL_PARAMS {
        let yaml =
            format!("name: collision\nparameters:\n  {name}: {{type: integer}}\nscript: echo ok");
        let error = parse_template_str(&yaml, Path::new("collision.yaml")).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(name), "{message}");
        assert!(message.contains("reserved for output control"), "{message}");
        assert!(message.contains("collision.yaml"), "{message}");
    }
}

#[test]
fn template_output_controls_are_separate_from_script_bindings() {
    let template = parse_template_str(
        "name: controls\nparameters:\n  message: {type: string, required: true}\ntemplate: true\nscript: '{{ message }}|{{ head_lines }}|{{ tail_lines }}|{{ max_output_bytes }}'",
        Path::new("controls.yaml"),
    ).unwrap();
    let args =
        json!({"message": "ok", "head_lines": 0, "tail_lines": 2, "max_output_bytes": 65536});
    let bound = template
        .validate_and_bind(args.as_object().unwrap())
        .unwrap();
    assert_eq!(bound.env, HashMap::from([("MESSAGE".into(), "ok".into())]));
    assert_eq!(
        bound.output,
        TemplateOutputOptions {
            head_lines: Some(0),
            tail_lines: Some(2),
            max_output_bytes: Some(65536),
        }
    );
    assert_eq!(template.render_script(&bound.env).unwrap(), "ok|||");
    for args in [
        json!({}),
        json!({"head_lines": null, "tail_lines": null, "max_output_bytes": null}),
    ] {
        assert_eq!(
            TemplateOutputOptions::from_args(args.as_object().unwrap()).unwrap(),
            TemplateOutputOptions::default()
        );
    }
}

#[test]
fn template_output_controls_reject_invalid_types_and_negative_values() {
    let template = parse_template_str(
        "name: controls\nscript: echo ok",
        Path::new("controls.yaml"),
    )
    .unwrap();
    for (name, _) in OUTPUT_CONTROL_PARAMS {
        for bad in [
            json!(-1),
            json!(1.5),
            json!("4"),
            json!(true),
            json!([]),
            json!({}),
        ] {
            let args = json!({name: bad});
            let error = template
                .validate_and_bind(args.as_object().unwrap())
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("invalid type for parameter `{name}`:")),
                "{error}"
            );
        }
    }
}
