use super::*;
use serde_json::{json, Value};

fn definition() -> Value {
    json!({
        "name": "issue_brief",
        "description": "Summarize an issue.",
        "parameters": {"type": "object"},
        "bash": "echo ok"
    })
}

#[test]
fn script_tool_defaults_and_config_data() {
    let value = definition();
    let tool: ScriptToolDefinition = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(tool.timeout, Duration::from_secs(300));
    assert_eq!(tool.output, ScriptToolOutput::Text);
    assert!(tool.tools.is_empty());
    assert_eq!(
        tool.source,
        ScriptToolSource::Bash {
            bash: "echo ok".into()
        }
    );
    let config: crate::config_data::ConfigData =
        serde_json::from_value(json!({"script_tools": [value]})).unwrap();
    assert_eq!(config.script_tools, [tool]);
    assert!(crate::config_data::ConfigData::default()
        .script_tools
        .is_empty());
}

#[test]
fn script_tool_python_duration_output_and_round_trip() {
    let mut value = definition();
    value.as_object_mut().unwrap().remove("bash");
    value["python"] = json!("print(tool_input())");
    value["timeout"] = json!("1m 250ms");
    value["output"] = json!("json");
    value["tools"] = json!(["fs_read", "fs_*", "toolset"]);
    let tool: ScriptToolDefinition = serde_json::from_value(value).unwrap();
    assert_eq!(tool.timeout, Duration::from_millis(60_250));
    assert_eq!(tool.output, ScriptToolOutput::Json);
    assert!(matches!(tool.source, ScriptToolSource::Python { .. }));
    let yaml = serde_yaml::to_string(&tool).unwrap();
    assert_eq!(tool, serde_yaml::from_str(&yaml).unwrap());
}

#[test]
fn script_tool_rejects_invalid_fields_with_name() {
    let cases = [
        ("python", json!("print('also')"), "exactly one"),
        ("unknown", json!(true), "unknown field"),
        ("script", json!("tool.sh"), "unknown field"),
        ("enabled", json!(false), "unknown field"),
        ("output", json!("yaml"), "unknown variant"),
        ("timeout", json!("0s"), "greater than zero"),
        ("timeout", json!("not a duration"), "invalid timeout"),
        ("timeout", json!(300), "expected a string"),
        ("bash", Value::Null, "must not be null"),
        ("tools", Value::Null, "invalid type"),
    ];
    for (field, bad_value, expected) in cases {
        let mut value = definition();
        value[field] = bad_value;
        let error = serde_json::from_value::<ScriptToolDefinition>(value)
            .unwrap_err()
            .to_string();
        assert!(error.contains("issue_brief"), "{field}: {error}");
        assert!(error.contains(expected), "{field}: {error}");
    }
    let mut value = definition();
    value.as_object_mut().unwrap().remove("bash");
    let error = serde_json::from_value::<ScriptToolDefinition>(value)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("issue_brief") && error.contains("exactly one"),
        "{error}"
    );
}

#[test]
fn script_tool_names_use_portable_charset_and_length() {
    for name in [
        "",
        "white space",
        "with.dot",
        "pkg/tool",
        "café",
        &"a".repeat(65),
    ] {
        assert!(validate_script_tool_name(name).is_err(), "{name}");
    }
    for name in ["AZaz09_-", "0", "-", &"a".repeat(64)] {
        assert!(validate_script_tool_name(name).is_ok(), "{name}");
    }
}
