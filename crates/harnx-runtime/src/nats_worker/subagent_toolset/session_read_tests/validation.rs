use super::*;

fn assert_numeric_bounds(property: &Value, minimum: u64, maximum: u64) {
    assert_eq!(
        (&property["minimum"], &property["maximum"]),
        (&json!(minimum), &json!(maximum))
    );
}

#[test]
fn resolve_defaults_apply_to_omitted_and_null_fields() {
    for read_options in [
        json!({}),
        json!({
            "index_min": null, "index_max": null, "types": null, "tool_name": null,
            "text_regex": null, "limit": null, "head_lines": null, "tail_lines": null,
            "max_output_bytes": null, "entry_index": null, "byte_offset": null,
        }),
    ] {
        let resolved = resolve(read_options).expect("defaults resolve");
        let ReadMode::Sequential(seq) = resolved.mode else {
            panic!("default read should be sequential");
        };
        assert_eq!((seq.index_min, seq.index_max), (None, None));
        assert_eq!(
            (
                seq.limit,
                seq.head_lines,
                seq.tail_lines,
                resolved.max_output_bytes
            ),
            (100, 25, 75, 16_384)
        );
    }
}

#[test]
fn resolve_rejects_limit_out_of_bounds() {
    assert_eq!(
        resolve(json!({ "limit": 0 })).unwrap_err(),
        "read.limit must be between 1 and 500"
    );
    assert_eq!(
        resolve(json!({ "limit": 501 })).unwrap_err(),
        "read.limit must be between 1 and 500"
    );
    resolve(json!({ "limit": 1 })).expect("limit 1 valid");
    resolve(json!({ "limit": 500 })).expect("limit 500 valid");
}

#[test]
fn resolve_rejects_max_output_bytes_out_of_bounds() {
    assert_eq!(
        resolve(json!({ "max_output_bytes": 1023 })).unwrap_err(),
        "read.max_output_bytes must be between 1024 and 65536"
    );
    assert_eq!(
        resolve(json!({ "max_output_bytes": 65537 })).unwrap_err(),
        "read.max_output_bytes must be between 1024 and 65536"
    );
    resolve(json!({ "max_output_bytes": 1024 })).expect("minimum budget valid");
    resolve(json!({ "max_output_bytes": 65536 })).expect("maximum budget valid");
}

#[test]
fn continuation_requires_the_offset_pair() {
    assert_eq!(
        resolve(json!({ "entry_index": 3 })).unwrap_err(),
        "read.entry_index and read.byte_offset must be provided together"
    );
    assert_eq!(
        resolve(json!({ "byte_offset": 8 })).unwrap_err(),
        "read.entry_index and read.byte_offset must be provided together"
    );
    let resolved = resolve(json!({ "entry_index": 3, "byte_offset": 8 })).expect("pair is valid");
    match resolved.mode {
        ReadMode::Continuation {
            entry_index,
            byte_offset,
        } => {
            assert_eq!(entry_index, 3);
            assert_eq!(byte_offset, 8);
        }
        other => panic!("expected continuation mode, got {other:?}"),
    }
}

#[test]
fn continuation_forbids_options_that_invalidate_offsets() {
    for (conflict, option) in [
        (json!({ "types": ["message"] }), "types"),
        (json!({ "tool_name": "bash" }), "tool_name"),
        (json!({ "text_regex": ".*" }), "text_regex"),
        (json!({ "index_min": 1 }), "index_min or read.index_max"),
        (json!({ "index_max": 9 }), "index_min or read.index_max"),
        (json!({ "head_lines": 5 }), "head_lines or read.tail_lines"),
        (json!({ "tail_lines": 5 }), "head_lines or read.tail_lines"),
    ] {
        let mut value = json!({ "entry_index": 3, "byte_offset": 8 });
        for (key, setting) in conflict.as_object().expect("object") {
            value[key] = setting.clone();
        }
        let error = resolve(value).unwrap_err();
        assert!(
            error.contains(&format!("cannot be combined with read.{option}")),
            "unexpected conflict error: {error}"
        );
    }
}

#[test]
fn invalid_regex_is_rejected_before_reading() {
    let error = resolve(json!({ "text_regex": "([" })).unwrap_err();
    assert!(
        error.starts_with("invalid read.text_regex:"),
        "unexpected regex error: {error}"
    );
    resolve(json!({ "text_regex": "^hello$" })).expect("valid regex accepted");
}

#[test]
fn unknown_read_fields_are_rejected() {
    let error = serde_json::from_value::<ReadOptions>(json!({ "frobnicate": true })).unwrap_err();
    assert!(
        error.to_string().contains("unknown field"),
        "expected unknown-field error, got: {error}"
    );
}

#[test]
fn schema_is_strict_at_every_level() {
    let schema = read_object_schema();
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_numeric_bounds(&schema["properties"]["limit"], 1, 500);
    assert_numeric_bounds(&schema["properties"]["max_output_bytes"], 1024, 65536);
    for option in ["index_min", "index_max", "entry_index", "byte_offset"] {
        assert_eq!(schema["properties"][option]["minimum"], json!(0));
    }
}
