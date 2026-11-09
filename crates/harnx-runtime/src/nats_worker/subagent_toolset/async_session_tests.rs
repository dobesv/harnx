//! Task 1 contract tests for the shared async send/wait schemas, wait
//! defaults, handle metadata budget, and the frozen result envelope.

use super::*;
use serde_json::json;

fn wait_options(value: Value) -> Result<WaitOptions, String> {
    let options: WaitOptions = serde_json::from_value(value).expect("wait options parse");
    options.resolve()?;
    Ok(options)
}

fn serialized(value: &Value) -> usize {
    serde_json::to_string(value)
        .expect("value serializes")
        .len()
}

#[test]
fn wait_defaults_apply_to_omitted_and_null_fields() {
    for wait in [
        json!({}),
        json!({ "seconds": null, "include_partial": null, "read": null }),
    ] {
        let resolved: WaitOptions = serde_json::from_value(wait).expect("wait options parse");
        let resolved = resolved.resolve().expect("defaults resolve");
        assert_eq!(resolved.seconds, DEFAULT_WAIT_SECONDS);
        assert_eq!(resolved.seconds, 120);
        assert!(resolved.include_partial);
        let super::super::session_read::ReadMode::Sequential(seq) = &resolved.read.mode else {
            panic!("default read should be sequential");
        };
        assert_eq!(seq.limit, 100);
        assert_eq!(resolved.read.max_output_bytes, 16_384);
    }
}

#[test]
fn wait_seconds_bounds_and_errors() {
    assert_eq!(
        wait_options(json!({ "seconds": 301 })).unwrap_err(),
        "wait.seconds must be at most 300"
    );
    let zero = wait_options(json!({ "seconds": 0 })).expect("zero is a valid snapshot wait");
    assert_eq!(zero.seconds, Some(0));
    let max = wait_options(json!({ "seconds": 300 })).expect("300 is valid");
    assert_eq!(max.seconds, Some(300));
    // Negative seconds are rejected at deserialization (u64).
    assert!(serde_json::from_value::<WaitOptions>(json!({ "seconds": -1 })).is_err());
}

#[test]
fn wait_validates_nested_read_options() {
    let error = wait_options(json!({ "read": { "limit": 0 } })).unwrap_err();
    assert_eq!(error, "read.limit must be between 1 and 500");
    let error = wait_options(json!({ "read": { "max_output_bytes": 65_537 } })).unwrap_err();
    assert_eq!(
        error,
        "read.max_output_bytes must be between 1024 and 65536"
    );
}

#[test]
fn wait_rejects_unknown_fields() {
    let error = serde_json::from_value::<WaitOptions>(json!({ "flavor": "patient" }))
        .expect_err("unknown field rejected");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn statuses_serialize_to_the_agreed_snake_case_strings() {
    let expected = [
        (SessionPromptStatus::Pending, "pending"),
        (SessionPromptStatus::Blocked, "blocked"),
        (SessionPromptStatus::Completed, "completed"),
        (SessionPromptStatus::Failed, "failed"),
        (SessionPromptStatus::Cancelled, "cancelled"),
        (SessionPromptStatus::TimedOut, "timed_out"),
        (SessionPromptStatus::Unknown, "unknown"),
    ];
    for (status, name) in expected {
        assert_eq!(
            serde_json::to_value(status).expect("serializes"),
            json!(name)
        );
    }
}

#[test]
fn dispatch_confirmation_serializes_to_confirmed_or_unconfirmed() {
    assert_eq!(
        serde_json::to_value(DispatchConfirmation::Confirmed).expect("serializes"),
        json!("confirmed")
    );
    assert_eq!(
        serde_json::to_value(DispatchConfirmation::Unconfirmed).expect("serializes"),
        json!("unconfirmed")
    );
}

#[test]
fn result_envelope_shape_is_frozen() {
    let envelope = SessionPromptResult {
        session_id: "child-1".to_string(),
        prompt_id: "prompt-9".to_string(),
        invocation_id: None,
        dispatch: Some(DispatchConfirmation::Confirmed),
        status: SessionPromptStatus::Pending,
        terminal: false,
        wait_expired: false,
        elapsed_ms: 1234,
        output: json!({ "events": [] }),
    };
    let value = serde_json::to_value(&envelope).expect("serializes");
    let keys: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "session_id",
            "prompt_id",
            "dispatch",
            "status",
            "terminal",
            "wait_expired",
            "elapsed_ms",
            "output"
        ]
    );
    assert_eq!(value["dispatch"], json!("confirmed"));
    assert_eq!(value["status"], json!("pending"));

    let unknown = SessionPromptResult {
        session_id: "child-1".to_string(),
        prompt_id: "prompt-9".to_string(),
        invocation_id: Some("inv-3".to_string()),
        dispatch: None,
        status: SessionPromptStatus::Unknown,
        terminal: false,
        wait_expired: true,
        elapsed_ms: 120_000,
        output: Value::Null,
    };
    let value = serde_json::to_value(&unknown).expect("serializes");
    assert_eq!(value["invocation_id"], json!("inv-3"));
    assert_eq!(value["dispatch"], json!(null));
    assert_eq!(value["wait_expired"], json!(true));
}

#[test]
fn result_metadata_rejects_identifiers_that_cannot_fit_their_handle() {
    assert!(ensure_result_metadata_fits("child-1", "prompt-9", 1024).is_ok());
    let error = ensure_result_metadata_fits(&"s".repeat(1_000), &"p".repeat(1_000), 1024)
        .expect_err("oversized identifiers must be rejected");
    assert!(
        error.contains("exceeds") && error.contains("max_output_bytes budget"),
        "unexpected budget error: {error}"
    );
    // The reserved size is worst-case envelope bytes, real metadata included.
    assert!(result_metadata_bytes("child-1", "prompt-9") < 200);
}

#[test]
fn parse_send_args_accepts_dispatch_only_and_explicit_wait() {
    let (args, wait) =
        parse_send_args(json!({ "message": "hello" })).expect("dispatch-only send parses");
    assert_eq!(args.message, "hello");
    assert!(wait.is_none(), "omitted wait is dispatch-only");

    let (args, wait) =
        parse_send_args(json!({ "message": "hello", "wait": null })).expect("null wait parses");
    assert!(args.message == "hello");
    assert!(wait.is_none(), "null wait is dispatch-only");

    let (_, wait) = parse_send_args(json!({ "message": "hello", "wait": {} }))
        .expect("explicit empty wait parses");
    let wait = wait.expect("explicit wait resolves");
    assert_eq!(wait.seconds, DEFAULT_WAIT_SECONDS);
    assert!(wait.include_partial);
}

#[test]
fn parse_send_args_rejects_unknown_fields_and_invalid_wait() {
    let error = parse_send_args(json!({
        "message": "hello",
        "frobnicate": true,
    }))
    .expect_err("unknown field rejected");
    assert!(error.starts_with("invalid session_send arguments"));

    let error = parse_send_args(json!({
        "message": "hello",
        "wait": { "seconds": 400 },
    }))
    .expect_err("oversized wait rejected");
    assert_eq!(error, "wait.seconds must be at most 300");

    let error = parse_send_args(json!({
        "message": "hello",
        "wait": { "read": { "entry_index": 1 } },
    }))
    .expect_err("invalid nested read rejected");
    assert!(
        error.contains("must be provided together"),
        "nested read errors surface through send: {error}"
    );
}

#[test]
fn parse_send_args_keeps_prompt_deadline_semantics() {
    // Nonpositive timeout inherits target policy, exactly like session_prompt.
    parse_send_args(json!({ "message": "hello", "timeout_secs": 0 }))
        .expect("zero timeout inherits");
    parse_send_args(json!({ "message": "hello", "timeout_secs": -5 }))
        .expect("negative timeout inherits");
    parse_send_args(json!({ "message": "hello", "timeout_secs": 60 }))
        .expect("positive timeout overrides");
}

#[test]
fn parse_send_args_rejects_session_ids_that_break_the_handle_budget() {
    let error = parse_send_args(json!({
        "message": "hello",
        "session_id": "x".repeat(1_000),
        "wait": { "read": { "max_output_bytes": 1024 } },
    }))
    .expect_err("oversized identifiers must fail before dispatch");
    assert!(
        error.contains("the handle could not be returned"),
        "unexpected handle-budget error: {error}"
    );
}

#[test]
fn parse_wait_args_defaults_and_validation() {
    let (args, wait) = parse_wait_args(json!({
        "session_id": "child-1",
        "prompt_id": "prompt-9",
    }))
    .expect("minimal wait parses");
    assert_eq!(args.session_id, "child-1");
    assert_eq!(args.prompt_id, "prompt-9");
    assert_eq!(
        wait.seconds, DEFAULT_WAIT_SECONDS,
        "omitted wait defaults to 120"
    );
    assert!(wait.include_partial);

    let (_, wait) = parse_wait_args(json!({
        "session_id": "child-1",
        "prompt_id": "prompt-9",
        "wait": { "seconds": 0 },
    }))
    .expect("immediate snapshot wait parses");
    assert_eq!(wait.seconds, 0);

    let (_, wait) = parse_wait_args(json!({
        "session_id": "child-1",
        "prompt_id": "prompt-9",
        "wait": { "seconds": 300 },
    }))
    .expect("explicit maximum parses");
    assert_eq!(wait.seconds, 300);

    assert_eq!(
        parse_wait_args(json!({ "session_id": "  ", "prompt_id": "p" })).unwrap_err(),
        "session_id must not be empty"
    );
    assert_eq!(
        parse_wait_args(json!({ "session_id": "s", "prompt_id": "" })).unwrap_err(),
        "prompt_id must not be empty"
    );
    let error = parse_wait_args(json!({ "session_id": "s", "prompt_id": "p", "why": 1 }))
        .expect_err("unknown field rejected");
    assert!(error.starts_with("invalid session_wait arguments"));
}

#[test]
fn parse_wait_args_rejects_handles_that_cannot_be_returned() {
    // With the caller's own small budget the handle cannot fit its envelope.
    let error = parse_wait_args(json!({
        "session_id": "s".repeat(1_000),
        "prompt_id": "p".repeat(1_000),
        "wait": { "read": { "max_output_bytes": 1024 } },
    }))
    .expect_err("oversized handle rejected");
    assert!(
        error.contains("the handle could not be returned"),
        "unexpected handle-budget error: {error}"
    );
}

#[test]
fn tool_schemas_are_strict_and_nested() {
    let send = session_send_spec("pkg/helper", None);
    assert_eq!(send.name, "session_send");
    assert_eq!(send.input_schema["required"], json!(["message"]));
    assert_eq!(send.input_schema["additionalProperties"], json!(false));
    let wait = &send.input_schema["properties"]["wait"]["anyOf"][0];
    assert_eq!(
        send.input_schema["properties"]["wait"]["anyOf"][1],
        json!({"type":"null"})
    );
    assert_eq!(wait["additionalProperties"], json!(false));
    assert_eq!(wait["properties"]["seconds"]["maximum"], json!(300));
    assert_eq!(wait["properties"]["seconds"]["minimum"], json!(0));
    assert_eq!(
        wait["properties"]["include_partial"]["type"],
        json!("boolean")
    );
    let read_property = &wait["properties"]["read"];
    assert_eq!(read_property["anyOf"][1], json!({"type":"null"}));
    let read = &read_property["anyOf"][0];
    assert_eq!(read["additionalProperties"], json!(false));
    assert_eq!(
        read["properties"]["max_output_bytes"]["maximum"],
        json!(65536)
    );
    assert_eq!(
        read["properties"]["max_output_bytes"]["minimum"],
        json!(1024)
    );

    let wait_tool = session_wait_spec("pkg/helper");
    assert_eq!(wait_tool.name, "session_wait");
    assert_eq!(
        wait_tool.input_schema["required"],
        json!(["session_id", "prompt_id"])
    );
    assert_eq!(wait_tool.input_schema["additionalProperties"], json!(false));
    let wait_property = &wait_tool.input_schema["properties"]["wait"];
    assert_eq!(wait_property["anyOf"][1], json!({"type":"null"}));
    assert_eq!(
        wait_property["anyOf"][0]["additionalProperties"],
        json!(false)
    );
}

#[test]
fn nullable_wait_and_nested_read_schema_match_parser() {
    // Specs come from production tool_specs, not an independent schema fixture.
    let tools = super::super::tool_specs("pkg/helper");
    let send = &tools[2].input_schema["properties"]["wait"];
    let wait = &tools[3].input_schema["properties"]["wait"];
    for schema in [send, wait] {
        assert_eq!(schema["anyOf"][1], json!({"type":"null"}));
        assert_eq!(schema["anyOf"][0]["additionalProperties"], json!(false));
    }
    let read = &send["anyOf"][0]["properties"]["read"];
    assert_eq!(read["anyOf"][1], json!({"type":"null"}));
    assert_eq!(read["anyOf"][0]["additionalProperties"], json!(false));
    assert!(parse_send_args(json!({"message":"x"})).is_ok());
    assert!(parse_send_args(json!({"message":"x", "wait":null})).is_ok());
    assert!(parse_wait_args(json!({"session_id":"s", "prompt_id":"p"})).is_ok());
    assert!(parse_wait_args(json!({"session_id":"s", "prompt_id":"p", "wait":null})).is_ok());
    assert!(wait_options(json!({"seconds":null, "include_partial":null, "read":null})).is_ok());
}

#[test]
fn send_and_wait_schemas_stay_within_their_bounds() {
    // Validation and the advertised schema must agree on the frozen bounds.
    let schema = read_object_schema();
    let limits = [
        (json!({ "limit": 500 }), true),
        (json!({ "limit": 501 }), false),
        (json!({ "max_output_bytes": 1024 }), true),
        (json!({ "max_output_bytes": 1023 }), false),
    ];
    assert_eq!(schema["properties"]["limit"]["maximum"], json!(500));
    assert_eq!(
        schema["properties"]["max_output_bytes"]["minimum"],
        json!(1024)
    );
    for (read_options, valid) in limits {
        assert_eq!(
            ReadOptions::resolve(
                &serde_json::from_value::<ReadOptions>(read_options).expect("read options parse")
            )
            .is_ok(),
            valid
        );
    }
    let wait_schema = wait_object_schema();
    assert_eq!(wait_schema["properties"]["seconds"]["maximum"], json!(300));
    assert!(wait_options(json!({ "seconds": 300 })).is_ok());
    assert!(wait_options(json!({ "seconds": 301 })).is_err());
}

#[test]
fn existing_toolset_is_untouched_by_the_async_contract() {
    // Task 5 wires discovery: the toolset now registers the async tools, but
    // `session_prompt` itself must keep its original synchronous contract.
    let tools = super::super::tool_specs("pkg/helper");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "session_new",
            "session_prompt",
            "session_send",
            "session_wait",
            "session_load",
            "session_cancel",
        ]
    );
    let prompt = &tools[1];
    assert!(
        prompt.input_schema["properties"].get("wait").is_none(),
        "session_prompt schema must not gain wait"
    );
    assert_eq!(prompt.input_schema["required"], json!(["message"]));
    assert_eq!(prompt.input_schema["additionalProperties"], json!(false));
    // Result shape of session_prompt is the synchronous response value.
    let sample = json!({
        "session_id": "child-1",
        "response": "done",
        "sub_agent": "pkg/helper",
    });
    assert!(serialized(&sample) > 0);
}
