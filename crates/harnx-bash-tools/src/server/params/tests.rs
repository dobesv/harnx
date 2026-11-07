use super::*;
use rmcp::schemars::generate::SchemaSettings;

fn schema<T: JsonSchema>() -> Value {
    let mut settings = SchemaSettings::default();
    settings.inline_subschemas = true;
    let schema = settings.into_generator().into_root_schema_for::<T>();
    serde_json::to_value(schema).expect("schema must serialize")
}

#[test]
fn spawn_without_wait_preserves_legacy_arguments() {
    let params: SpawnCommandParams = serde_json::from_value(json!({
        "command": "echo hi",
        "working_dir": "/tmp",
        "env": {"MODE": "test"}
    }))
    .unwrap();
    assert_eq!(params.command, "echo hi");
    assert_eq!(params.working_dir.as_deref(), Some("/tmp"));
    assert_eq!(params.env.unwrap().get("MODE").unwrap(), "test");
    assert_eq!(params.wait, None);

    let minimal: SpawnCommandParams =
        serde_json::from_value(json!({"command": "echo hi"})).unwrap();
    assert_eq!(minimal.wait, None);
    assert_eq!(minimal.working_dir, None);
    assert_eq!(minimal.env, None);
}

#[test]
fn empty_nested_wait_and_flat_wait_share_defaults() {
    let spawn: SpawnCommandParams =
        serde_json::from_value(json!({"command": "echo hi", "wait": {}})).unwrap();
    let wait: WaitParams = serde_json::from_value(json!({"execution_id": "exec-123"})).unwrap();
    let options = spawn.wait.expect("empty wait enables waiting");
    assert_eq!(options, WaitOptions::default());
    assert_eq!(wait.execution_id, "exec-123");
    assert_eq!(wait.options, options);
    assert_eq!(options.effective_timeout_secs(), 120);
}

fn assert_nested_and_flat_wait_options(
    options: Value,
    expected: WaitOptions,
    effective_timeout_secs: u64,
) {
    let spawn: SpawnCommandParams =
        serde_json::from_value(json!({"command": "echo hi", "wait": options})).unwrap();
    let mut flat = options;
    flat["execution_id"] = json!("exec-123");
    let wait: WaitParams = serde_json::from_value(flat).unwrap();

    assert_eq!(spawn.wait, Some(expected));
    assert_eq!(spawn.wait.unwrap(), wait.options);
    assert_eq!(
        wait.options.effective_timeout_secs(),
        effective_timeout_secs
    );
}

#[test]
fn populated_and_zero_wait_options_match_flat_wait_options() {
    let cases = [
        (
            json!({
                "timeout_secs": 30,
                "head_lines": 4,
                "tail_lines": 7,
                "max_output_bytes": 512,
                "grep": "error|warning"
            }),
            WaitOptions {
                timeout_secs: Some(30),
                head_lines: Some(4),
                tail_lines: Some(7),
                max_output_bytes: Some(512),
                grep: Some("error|warning".to_string()),
            },
            30,
        ),
        (
            json!({
                "timeout_secs": 0,
                "head_lines": 0,
                "tail_lines": 0,
                "max_output_bytes": 0,
                "grep": ""
            }),
            WaitOptions {
                timeout_secs: Some(0),
                head_lines: Some(0),
                tail_lines: Some(0),
                max_output_bytes: Some(0),
                grep: Some(String::new()),
            },
            0,
        ),
    ];

    for (input, expected, effective_timeout) in cases {
        assert_nested_and_flat_wait_options(input, expected, effective_timeout);
    }
}

#[test]
fn null_wait_options_match_omitted_options() {
    let spawn: SpawnCommandParams =
        serde_json::from_value(json!({"command": "echo hi", "wait": null})).unwrap();
    assert_eq!(spawn.wait, None);

    let options = json!({
        "timeout_secs": null,
        "head_lines": null,
        "tail_lines": null,
        "max_output_bytes": null,
        "grep": null
    });
    let spawn: SpawnCommandParams =
        serde_json::from_value(json!({"command": "echo hi", "wait": options})).unwrap();
    let mut flat = options;
    flat["execution_id"] = json!("exec-123");
    let wait: WaitParams = serde_json::from_value(flat).unwrap();
    assert_eq!(spawn.wait, Some(WaitOptions::default()));
    assert_eq!(wait.options, WaitOptions::default());
}

#[test]
fn nested_and_flat_wait_reject_invalid_option_types() {
    for field in [
        "timeout_secs",
        "head_lines",
        "tail_lines",
        "max_output_bytes",
    ] {
        for value in [json!(-1), json!("1"), json!(1.5)] {
            let mut options = json!({});
            options[field] = value;
            assert!(serde_json::from_value::<SpawnCommandParams>(json!({
                "command": "echo hi", "wait": options
            }))
            .is_err());
            options["execution_id"] = json!("exec-123");
            assert!(serde_json::from_value::<WaitParams>(options).is_err());
        }
    }
    assert!(serde_json::from_value::<SpawnCommandParams>(json!({
        "command": "echo hi", "wait": 30
    }))
    .is_err());
    assert!(serde_json::from_value::<WaitParams>(json!({})).is_err());
}

#[test]
fn nested_wait_schema_matches_flat_wait_properties() {
    let spawn = schema::<SpawnCommandParams>();
    let wait = schema::<WaitParams>();
    let wait_property = &spawn["properties"]["wait"];
    // Schemars can represent nullable objects with a type list or anyOf.
    let nested = wait_property
        .get("anyOf")
        .and_then(Value::as_array)
        .and_then(|variants| {
            variants
                .iter()
                .find(|variant| variant.get("properties").is_some())
        })
        .unwrap_or(wait_property);
    let nested_properties = nested["properties"].as_object().unwrap();
    let mut flat_properties = wait["properties"].as_object().unwrap().clone();
    flat_properties.remove("execution_id").unwrap();
    assert_eq!(nested_properties, &flat_properties);
    assert_eq!(nested_properties.len(), 5);
    assert!(!nested_properties.contains_key("execution_id"));
    assert_eq!(nested["additionalProperties"], false);
    assert!(nested.get("required").is_none());
    assert_eq!(spawn["required"], json!(["command"]));
    assert_eq!(wait["required"], json!(["execution_id"]));
    assert!(wait["properties"].get("options").is_none());
    assert!(spawn["properties"].get("wait_secs").is_none());
    assert!(wait["properties"].get("wait_secs").is_none());

    for field in [
        "timeout_secs",
        "head_lines",
        "tail_lines",
        "max_output_bytes",
    ] {
        assert_eq!(nested_properties[field]["type"], json!(["integer", "null"]));
        assert_eq!(nested_properties[field]["minimum"], 0);
    }
    assert_eq!(nested_properties["grep"]["type"], json!(["string", "null"]));
    assert!(
        wait_property["type"]
            .as_array()
            .is_some_and(|types| types.contains(&json!("null")))
            || wait_property["anyOf"]
                .as_array()
                .is_some_and(|variants| variants.iter().any(|variant| variant["type"] == "null"))
    );
}
