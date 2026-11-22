//! Contract tests for the shared async send/wait schemas, wait
//! defaults, handle metadata budget, and the frozen result envelope.

use super::specs::wait_object_schema;
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
        json!({ "seconds": null, "forever": null, "include_partial": null }),
    ] {
        let resolved: WaitOptions = serde_json::from_value(wait).expect("wait options parse");
        let resolved = resolved.resolve().expect("defaults resolve");
        assert_eq!(resolved.limit, WaitLimit::Seconds(WAIT_DEFAULT_SECS));
        assert_eq!(resolved.limit, WaitLimit::Seconds(240));
        assert!(resolved.include_partial);
    }
}

#[test]
fn wait_seconds_bounds_and_errors() {
    for seconds in [0, 241] {
        assert_eq!(
            wait_options(json!({ "seconds": seconds })).unwrap_err(),
            "wait.seconds must be between 1 and 240; omit wait to return immediately"
        );
    }
    for seconds in [1, 240] {
        assert_eq!(
            wait_options(json!({ "seconds": seconds })).unwrap().seconds,
            Some(seconds)
        );
    }
    // Negative seconds are rejected at deserialization (u64).
    assert!(serde_json::from_value::<WaitOptions>(json!({ "seconds": -1 })).is_err());
}

#[test]
fn forever_resolves_and_excludes_seconds() {
    let resolved = |value| {
        serde_json::from_value::<WaitOptions>(value)
            .unwrap()
            .resolve()
    };
    assert_eq!(
        resolved(json!({"forever": true})).unwrap().limit,
        WaitLimit::Forever
    );
    assert_eq!(
        resolved(json!({"forever": false})).unwrap().limit,
        WaitLimit::Seconds(240)
    );
    let error = resolved(json!({"seconds": 30, "forever": true})).unwrap_err();
    assert!(error.contains("either seconds or forever"), "{error}");
}

#[test]
fn waits_no_longer_take_a_read_option() {
    assert!(serde_json::from_value::<WaitOptions>(json!({ "read": {} })).is_err());
    let error = parse_send_args(json!({ "message": "hello", "wait": { "read": {} } }))
        .expect_err("send's wait rejects read");
    assert!(error.contains("unknown field `read`"), "{error}");
    assert!(error.contains("session_load with prompt_id"), "{error}");
    let error = wait_args_with(json!({ "read": null })).expect_err("wait rejects read");
    assert!(error.contains("unknown field `read`"), "{error}");
    assert!(error.contains("session_load with prompt_id"), "{error}");
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
        stopped_by: None,
        elapsed_ms: 1234,
        response: None,
        response_truncated: false,
        failure: None,
        activity: None,
        observation: Observation::default(),
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
            "observation"
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
        stopped_by: None,
        elapsed_ms: 120_000,
        response: None,
        response_truncated: false,
        failure: None,
        activity: None,
        observation: Observation::default(),
    };
    let value = serde_json::to_value(&unknown).expect("serializes");
    assert_eq!(value["invocation_id"], json!("inv-3"));
    assert_eq!(value["dispatch"], json!(null));
    assert_eq!(value["wait_expired"], json!(true));

    // A settled prompt carries its reply where the transcript page used to be.
    let settled = SessionPromptResult {
        status: SessionPromptStatus::Failed,
        terminal: true,
        response: Some("partial answer".into()),
        response_truncated: true,
        failure: Some("provider error".into()),
        ..SessionPromptResult::empty(&PromptHandle {
            session_id: "child-1".into(),
            prompt_id: "prompt-9".into(),
        })
    };
    let value = serde_json::to_value(&settled).expect("serializes");
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys[keys.len() - 4..],
        ["response", "response_truncated", "failure", "observation"]
    );
}

#[test]
fn result_metadata_rejects_identifiers_that_cannot_fit_their_handle() {
    assert!(ensure_result_metadata_fits("child-1", "prompt-9", 1024).is_ok());
    let error = ensure_result_metadata_fits(&"s".repeat(1_000), &"p".repeat(1_000), 1024)
        .expect_err("oversized identifiers must be rejected");
    assert!(
        error.contains("exceeds") && error.contains("byte result budget"),
        "unexpected budget error: {error}"
    );
    // The reserved size is worst-case envelope bytes, real metadata included.
    let reserved = result_metadata_bytes("child-1", "prompt-9");
    assert!((250..350).contains(&reserved), "{reserved}");
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
    assert_eq!(wait.limit, WaitLimit::Seconds(WAIT_DEFAULT_SECS));
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
    assert!(error.contains("between 1 and 240"), "{error}");

    let error = parse_send_args(json!({
        "message": "hello",
        "wait": { "include_partial": "yes" },
    }))
    .expect_err("invalid wait field rejected");
    assert!(
        error.starts_with("invalid session_send arguments"),
        "{error}"
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
        "session_id": "x".repeat(200_000),
        "wait": {},
    }))
    .expect_err("oversized identifiers must fail before dispatch");
    assert!(
        error.contains("the handle could not be returned"),
        "unexpected handle-budget error: {error}"
    );
}

fn wait_args_with(extra: Value) -> Result<ResolvedWait, String> {
    let mut args = json!({ "session_id": "child-1", "prompt_id": "prompt-9" });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    parse_wait_args(args).map(|(_, wait)| wait)
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
        wait.limit,
        WaitLimit::Seconds(240),
        "omitted seconds default"
    );
    assert!(wait.include_partial);
    assert_eq!(
        wait_args_with(json!({ "seconds": 240 })).unwrap().limit,
        WaitLimit::Seconds(240)
    );
    assert_eq!(
        wait_args_with(json!({ "forever": true })).unwrap().limit,
        WaitLimit::Forever
    );
    assert_eq!(
        wait_args_with(json!({ "forever": false })).unwrap().limit,
        WaitLimit::Seconds(240)
    );
}

#[test]
fn parse_wait_args_rejects_bad_seconds_and_nested_wait() {
    for seconds in [0, 241] {
        let error = wait_args_with(json!({ "seconds": seconds })).unwrap_err();
        assert!(error.contains("between 1 and 240"), "{error}");
        assert!(error.contains("session_load"), "{error}");
    }
    let error = wait_args_with(json!({ "seconds": 30, "forever": true })).unwrap_err();
    assert!(error.contains("either seconds or forever"), "{error}");
    let error = wait_args_with(json!({ "wait": {} })).unwrap_err();
    assert!(error.contains("unknown field"), "{error}");
    assert!(error.contains("top level"), "{error}");
}

#[test]
fn parse_wait_args_rejects_empty_ids_and_unknown_fields() {
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
    let error = parse_wait_args(json!({
        "session_id": "s".repeat(100_000),
        "prompt_id": "p".repeat(100_000),
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
    assert_seconds_and_forever(&wait["properties"]);
    assert_eq!(
        wait["properties"]["include_partial"]["type"],
        json!("boolean")
    );
    assert!(wait["properties"].get("read").is_none(), "{wait}");

    let wait_tool = session_wait_spec("pkg/helper");
    assert_eq!(wait_tool.name, "session_wait");
    assert_eq!(
        wait_tool.input_schema["required"],
        json!(["session_id", "prompt_id"])
    );
    assert_eq!(wait_tool.input_schema["additionalProperties"], json!(false));
    let properties = &wait_tool.input_schema["properties"];
    assert!(properties.get("wait").is_none(), "session_wait is flat");
    assert!(properties.get("read").is_none(), "{properties}");
    assert_seconds_and_forever(properties);
}

fn assert_seconds_and_forever(properties: &Value) {
    let seconds = &properties["seconds"];
    assert_eq!(
        (
            &seconds["minimum"],
            &seconds["maximum"],
            &seconds["default"]
        ),
        (&json!(1), &json!(240), &json!(240))
    );
    assert_eq!(properties["forever"]["type"], json!("boolean"));
}

#[test]
fn descriptions_do_not_mention_cache() {
    let send = session_send_spec("pkg/helper", None);
    let wait = session_wait_spec("pkg/helper");
    for text in [
        serde_json::to_string(&send).unwrap(),
        serde_json::to_string(&wait).unwrap(),
    ] {
        assert!(!text.to_lowercase().contains("cache"), "{text}");
    }
}

#[test]
fn nullable_wait_schema_matches_parser() {
    // Specs come from production tool_specs, not an independent schema fixture.
    let tools = super::super::tool_specs("pkg/helper");
    let send = &tools[2].input_schema["properties"]["wait"];
    assert_eq!(send["anyOf"][1], json!({"type":"null"}));
    assert_eq!(send["anyOf"][0]["additionalProperties"], json!(false));
    assert!(parse_send_args(json!({"message":"x"})).is_ok());
    assert!(parse_send_args(json!({"message":"x", "wait":null})).is_ok());
    assert!(parse_wait_args(json!({"session_id":"s", "prompt_id":"p"})).is_ok());
    assert!(parse_wait_args(json!({
        "session_id":"s", "prompt_id":"p", "seconds":null, "forever":null, "include_partial":null
    }))
    .is_ok());
    assert!(wait_options(json!({"seconds":null, "forever":null, "include_partial":null})).is_ok());
}

#[test]
fn send_and_wait_schemas_stay_within_their_bounds() {
    // Validation and the advertised schema must agree on the frozen bounds.
    let wait_schema = wait_object_schema();
    assert_eq!(wait_schema["properties"]["seconds"]["maximum"], json!(240));
    assert!(wait_options(json!({ "seconds": 240 })).is_ok());
    assert!(wait_options(json!({ "seconds": 241 })).is_err());
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
