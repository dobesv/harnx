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
    assert_eq!(wait.options(), options);
    assert_eq!(options.limit().unwrap(), WaitLimit::Seconds(240));
    assert_eq!(wait.limit().unwrap(), WaitLimit::Seconds(240));
}

#[test]
fn nested_and_flat_wait_options_match() {
    let cases = [
        (
            json!({"seconds": 30, "head_lines": 4, "tail_lines": 7,
                "max_output_bytes": 512, "grep": "error|warning"}),
            WaitLimit::Seconds(30),
        ),
        (
            json!({"forever": true, "head_lines": 0}),
            WaitLimit::Forever,
        ),
        (
            json!({"seconds": null, "forever": null, "head_lines": null, "tail_lines": null,
                "max_output_bytes": null, "grep": null}),
            WaitLimit::Seconds(240),
        ),
    ];
    for (input, limit) in cases {
        let spawn: SpawnCommandParams =
            serde_json::from_value(json!({"command": "echo hi", "wait": input})).unwrap();
        let mut flat = input;
        flat["execution_id"] = json!("exec-123");
        let wait: WaitParams = serde_json::from_value(flat).unwrap();
        let nested = spawn.wait.unwrap();
        assert_eq!(nested, wait.options());
        assert_eq!(nested.limit().unwrap(), limit);
        assert_eq!(wait.limit().unwrap(), limit);
    }
}

#[test]
fn null_wait_matches_omitted_wait() {
    let spawn: SpawnCommandParams =
        serde_json::from_value(json!({"command": "echo hi", "wait": null})).unwrap();
    assert_eq!(spawn.wait, None);
}

#[test]
fn wait_seconds_outside_range_or_with_forever_is_rejected() {
    for seconds in [0, 241] {
        let options = WaitOptions {
            seconds: Some(seconds),
            ..WaitOptions::default()
        };
        let message = options.limit().unwrap_err().message;
        assert!(message.contains("between 1 and 240"), "{message}");
        assert!(message.contains("omit wait"), "{message}");

        let wait: WaitParams =
            serde_json::from_value(json!({"execution_id": "x", "seconds": seconds})).unwrap();
        let message = wait.limit().unwrap_err().message;
        assert!(message.contains("between 1 and 240"), "{message}");
        assert!(message.contains("bash_status"), "{message}");
    }
    let both = WaitOptions {
        seconds: Some(5),
        forever: Some(true),
        ..WaitOptions::default()
    };
    assert!(both.limit().is_err());
    let forever_false = WaitOptions {
        seconds: Some(5),
        forever: Some(false),
        ..WaitOptions::default()
    };
    assert_eq!(forever_false.limit().unwrap(), WaitLimit::Seconds(5));
}

#[test]
fn old_and_unknown_arguments_are_rejected() {
    let nested = serde_json::from_value::<SpawnCommandParams>(json!({
        "command": "echo hi", "wait": {"timeout_secs": 30}
    }))
    .unwrap_err();
    assert!(nested.to_string().contains("unknown field"), "{nested}");
    let flat = serde_json::from_value::<WaitParams>(json!({
        "execution_id": "x", "timeout_secs": 30
    }))
    .unwrap_err();
    assert!(flat.to_string().contains("unknown field"), "{flat}");
    let status = serde_json::from_value::<StatusParams>(json!({
        "execution_id": "x", "seconds": 3
    }))
    .unwrap_err();
    assert!(status.to_string().contains("unknown field"), "{status}");
    assert!(serde_json::from_value::<SpawnCommandParams>(json!({
        "command": "echo hi", "bogus": 1
    }))
    .is_err());
}

#[test]
fn nested_and_flat_wait_reject_invalid_option_types() {
    for field in ["seconds", "head_lines", "tail_lines", "max_output_bytes"] {
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
    assert_eq!(nested_properties.len(), 6);
    assert!(!nested_properties.contains_key("execution_id"));
    assert_eq!(nested["additionalProperties"], false);
    assert!(nested.get("required").is_none());
    assert_eq!(spawn["required"], json!(["command"]));
    assert_eq!(wait["required"], json!(["execution_id"]));
    assert!(wait["properties"].get("options").is_none());
    assert!(spawn["properties"].get("wait_secs").is_none());
    assert!(wait["properties"].get("wait_secs").is_none());

    for field in ["seconds", "head_lines", "tail_lines", "max_output_bytes"] {
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

#[test]
fn spawn_schema_has_timeout_and_every_params_schema_is_closed() {
    let spawn = schema::<SpawnCommandParams>();
    assert_eq!(
        spawn["properties"]["timeout_secs"]["type"],
        json!(["integer", "null"])
    );
    let status = schema::<StatusParams>();
    assert_eq!(status["required"], json!(["execution_id"]));
    assert!(status["properties"].get("seconds").is_none());
    for schema in [&spawn, &status, &schema::<WaitParams>()] {
        assert_eq!(schema["additionalProperties"], false);
    }
}
