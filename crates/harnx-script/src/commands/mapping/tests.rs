use serde_json::{json, Value};

use super::*;

fn mapping() -> OutputMapping {
    OutputMapping {
        stdout: Some("out".into()),
        row: Some("{path}:{line}:{text}".into()),
        stderr: Some("notes".into()),
        exit_code: Some("exit_code".into()),
        ..Default::default()
    }
}

fn result(structured: Value) -> Value {
    json!({"content":[{"type":"text","text":"ignored"}],"structuredContent":structured})
}

fn apply_text(structured: Value) -> Mapped {
    apply("fs_grep", &mapping(), &result(structured), None).unwrap()
}

#[test]
fn string_stdout_is_printed_without_an_added_newline() {
    assert_eq!(apply_text(json!({"out":"abc"})).stdout, "abc");
    assert_eq!(apply_text(json!({"out":"abc\n"})).stdout, "abc\n");
    assert_eq!(apply_text(json!({"out":"a\nb"})).stdout, "a\nb");
}

#[test]
fn array_of_strings_prints_one_per_line() {
    assert_eq!(apply_text(json!({"out":["a","b"]})).stdout, "a\nb\n");
}

#[test]
fn array_of_objects_prints_one_formatted_row_per_line() {
    let mapped = apply_text(json!({"out":[
        {"path":"a.rs","line":1,"text":"x"},
        {"path":"b.rs","line":22,"text":"y"}]}));
    assert_eq!(mapped.stdout, "a.rs:1:x\nb.rs:22:y\n");
}

#[test]
fn row_missing_field_is_empty_and_scalar_is_json_text() {
    let mapped = apply_text(json!({"out":[{"path":"a.rs","line":true}]}));
    assert_eq!(mapped.stdout, "a.rs:true:\n");
}

#[test]
fn row_keeps_colons_and_spaces_in_values() {
    let mapped = apply_text(json!({"out":[{"path":"a b:c.rs","line":3,"text":"x: y"}]}));
    assert_eq!(mapped.stdout, "a b:c.rs:3:x: y\n");
}

#[test]
fn empty_string_and_empty_array_print_nothing() {
    assert_eq!(apply_text(json!({"out":""})).stdout, "");
    assert_eq!(apply_text(json!({"out":[]})).stdout, "");
}

#[test]
fn missing_stdout_field_prints_json() {
    let mapped = apply_text(json!({"other":1}));
    assert_eq!(mapped.stdout, "{\"other\":1}\n");
}

#[test]
fn other_shapes_print_json() {
    assert_eq!(apply_text(json!({"out":7})).stdout, "{\"out\":7}\n");
    assert_eq!(apply_text(json!({"out":[1,2]})).stdout, "{\"out\":[1,2]}\n");
}

#[test]
fn stderr_notes_are_prefixed_one_per_line() {
    assert_eq!(
        apply_text(json!({"out":"","notes":["a","b"]})).stderr,
        "fs_grep: a\nfs_grep: b\n"
    );
    assert_eq!(apply_text(json!({"notes":"one"})).stderr, "fs_grep: one\n");
}

#[test]
fn wrong_shaped_stderr_field_prints_nothing() {
    assert_eq!(apply_text(json!({"notes":5})).stderr, "");
    assert_eq!(apply_text(json!({})).stderr, "");
}

#[test]
fn exit_code_field_is_used_and_clamped() {
    assert_eq!(apply_text(json!({"exit_code":1})).exit_code, 1);
    assert_eq!(apply_text(json!({"exit_code":999})).exit_code, 255);
}

#[test]
fn exit_status_outside_the_shell_range_is_255() {
    for code in [-1, 256, 70000] {
        assert_eq!(apply_text(json!({"exit_code": code})).exit_code, 255);
    }
    for code in [0, 1, 255] {
        assert_eq!(apply_text(json!({"exit_code": code})).exit_code, code);
    }
}

#[test]
fn exit_status_too_large_for_i64_is_255() {
    let status = json!({"exit_code": 9_223_372_036_854_775_808_u64});
    assert_eq!(apply_text(status).exit_code, 255);
}

fn stderr_text_mapping() -> OutputMapping {
    OutputMapping {
        stderr_text: Some("err".into()),
        ..mapping()
    }
}

fn mapped_stderr(structured: Value, format: Option<OutputFormat>) -> String {
    apply("tool", &stderr_text_mapping(), &result(structured), format)
        .unwrap()
        .stderr
}

#[test]
fn stderr_text_is_printed_verbatim_before_notes() {
    let structured = json!({"err": "a\nb\n", "notes": ["cut"]});
    assert_eq!(mapped_stderr(structured, None), "a\nb\ntool: cut\n");
}

#[test]
fn stderr_text_without_notes_is_exact() {
    let structured = json!({"err": "a\nb", "notes": []});
    assert_eq!(mapped_stderr(structured, None), "a\nb");
}

#[test]
fn a_note_after_unterminated_stderr_text_starts_its_own_line() {
    let structured = json!({"err": "a", "notes": ["cut"]});
    assert_eq!(mapped_stderr(structured, None), "a\ntool: cut\n");
}

#[test]
fn stderr_text_missing_or_not_a_string_prints_nothing() {
    assert_eq!(mapped_stderr(json!({"out": "x"}), None), "");
    assert_eq!(mapped_stderr(json!({"err": 5}), None), "");
    assert_eq!(
        mapped_stderr(json!({"err": ["a"], "notes": ["n"]}), None),
        "tool: n\n"
    );
}

#[test]
fn json_format_keeps_stderr_text() {
    let structured = json!({"err": "e", "notes": ["n"], "out": "o"});
    let mapped = apply(
        "tool",
        &stderr_text_mapping(),
        &result(structured),
        Some(OutputFormat::Json),
    )
    .unwrap();
    assert_eq!(
        mapped.stdout,
        "{\"err\":\"e\",\"notes\":[\"n\"],\"out\":\"o\"}\n"
    );
    assert_eq!(mapped.stderr, "e\ntool: n\n");
}

#[test]
fn missing_or_non_integer_exit_code_is_zero() {
    assert_eq!(apply_text(json!({})).exit_code, 0);
    assert_eq!(apply_text(json!({"exit_code":"1"})).exit_code, 0);
    let unnamed = apply(
        "t",
        &OutputMapping::default(),
        &result(json!({"exit_code":3})),
        None,
    );
    assert_eq!(unnamed.unwrap().exit_code, 0);
}

#[test]
fn json_format_prints_all_fields_and_keeps_stderr_and_exit() {
    let value = json!({"structuredContent":{"matches":[],"notes":["n"],"exit_code":1}});
    let mapped = apply("fs_grep", &mapping(), &value, Some(OutputFormat::Json)).unwrap();
    assert_eq!(
        mapped.stdout,
        "{\"matches\":[],\"notes\":[\"n\"],\"exit_code\":1}\n"
    );
    assert_eq!(mapped.stderr, "fs_grep: n\n");
    assert_eq!(mapped.exit_code, 1);
}

#[test]
fn no_structured_content_is_not_mapped() {
    let value = json!({"content":[{"type":"text","text":"plain"}]});
    assert!(apply("t", &mapping(), &value, None).is_none());
    let non_object = json!({"structuredContent":[1]});
    assert!(apply("t", &mapping(), &non_object, None).is_none());
}

#[test]
fn error_exit_code_is_kept_to_the_range_a_shell_reports() {
    let with = |code| OutputMapping {
        error_exit_code: Some(code),
        ..Default::default()
    };
    assert_eq!(with(7).error_exit(), Some(7));
    assert_eq!(with(300).error_exit(), Some(255));
    assert_eq!(with(-4).error_exit(), Some(1));
    assert_eq!(with(0).error_exit(), Some(1));
    assert_eq!(OutputMapping::default().error_exit(), None);
}
