use serde_json::{json, Value};

use super::*;

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "limit": {"type": "integer"},
            "pattern": {"type": "string"},
            "from": {"type": "integer"},
            "max-depth": {"type": "integer"}
        },
        "required": ["path", "pattern"]
    })
}

fn bind(args: Vec<Value>, kwargs: Vec<(&str, Value)>) -> Result<Value, String> {
    let kwargs = kwargs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    bind_python_call("fs_grep", &schema(), args, kwargs).map_err(|e| e.0)
}

fn names(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_prelude_is_two_lines_for_any_number_of_tools() {
    let two = tools_prelude(&names(&["fs_read", "fs_grep"]));
    assert_eq!(
        two,
        "class tools: pass\n\
         _harnx_tool = lambda n: (lambda *a, **k: _harnx_call(n, list(a), k)); tools = tools(); [setattr(tools, n, _harnx_tool(n)) for n in [\"fs_read\", \"fs_grep\"]]"
    );
    assert_eq!(tools_prelude(&[]).matches('\n').count(), 1);
    assert!(tools_prelude(&[]).ends_with("for n in []]"));
    assert_eq!(PYTHON_CALL_FUNCTION, "_harnx_call");
}

#[test]
fn the_prelude_escapes_quotes_and_backslashes() {
    let prelude = tools_prelude(&names(&["a\"b", "c\\d"]));
    assert!(prelude.ends_with(r#"for n in ["a\"b", "c\\d"]]"#));
    assert_eq!(prelude.matches('\n').count(), 1);
}

#[test]
fn keywords_alone_bind_in_schema_order() {
    let bound = bind(vec![], vec![("pattern", json!("x")), ("path", json!("."))]).unwrap();
    assert_eq!(
        serde_json::to_string(&bound).unwrap(),
        r#"{"path":".","pattern":"x"}"#
    );
}

#[test]
fn positionals_fill_required_properties_in_schema_order() {
    let bound = bind(vec![json!("."), json!("x")], vec![]).unwrap();
    assert_eq!(bound, json!({"path": ".", "pattern": "x"}));
}

#[test]
fn a_positional_and_a_keyword_can_mix() {
    let bound = bind(
        vec![json!(".")],
        vec![("pattern", json!("x")), ("limit", json!(3))],
    )
    .unwrap();
    assert_eq!(
        serde_json::to_string(&bound).unwrap(),
        r#"{"path":".","limit":3,"pattern":"x"}"#
    );
}

#[test]
fn too_many_positionals_is_an_error() {
    let err = bind(vec![json!("a"), json!("b"), json!("c")], vec![]).unwrap_err();
    assert_eq!(
        err,
        "fs_grep() takes at most 2 positional arguments but 3 were given"
    );
    let single = json!({"properties": {"path": {}}, "required": ["path"]});
    let err = bind_python_call("fs_read", &single, vec![json!(1), json!(2)], vec![]).unwrap_err();
    assert_eq!(
        err.0,
        "fs_read() takes at most 1 positional argument but 2 were given"
    );
}

#[test]
fn a_missing_required_property_is_named() {
    let err = bind(vec![], vec![("pattern", json!("x"))]).unwrap_err();
    assert_eq!(err, "fs_grep() missing 1 required argument: 'path'");
    let err = bind(vec![], vec![]).unwrap_err();
    assert_eq!(
        err,
        "fs_grep() missing 2 required arguments: 'path' and 'pattern'"
    );
}

#[test]
fn three_missing_properties_are_listed_with_commas() {
    let schema = json!({"properties": {"a": {}, "b": {}, "c": {}}, "required": ["a", "b", "c"]});
    let err = bind_python_call("t", &schema, vec![], vec![]).unwrap_err();
    assert_eq!(err.0, "t() missing 3 required arguments: 'a', 'b' and 'c'");
}

#[test]
fn an_unknown_keyword_is_an_error() {
    let err = bind(vec![json!("."), json!("x")], vec![("bogus", json!(1))]).unwrap_err();
    assert_eq!(err, "fs_grep() got an unexpected keyword argument 'bogus'");
}

#[test]
fn a_property_given_both_ways_is_an_error() {
    let err = bind(vec![json!(".")], vec![("path", json!("."))]).unwrap_err();
    assert_eq!(err, "fs_grep() got multiple values for argument 'path'");
}

#[test]
fn python_keywords_and_dashed_names_bind_by_their_exact_key() {
    let bound = bind(
        vec![json!("."), json!("x")],
        vec![("from", json!(1)), ("max-depth", json!(2))],
    )
    .unwrap();
    assert_eq!(bound["from"], json!(1));
    assert_eq!(bound["max-depth"], json!(2));
    let err = bind(vec![json!("."), json!("x")], vec![("max_depth", json!(2))]).unwrap_err();
    assert_eq!(
        err,
        "fs_grep() got an unexpected keyword argument 'max_depth'"
    );
}

#[test]
fn none_for_an_optional_property_is_dropped() {
    let bound = bind(vec![json!("."), json!("x")], vec![("limit", Value::Null)]).unwrap();
    assert_eq!(bound, json!({"path": ".", "pattern": "x"}));
}

#[test]
fn a_wrong_type_names_the_property() {
    let err = bind(vec![json!("."), json!("x")], vec![("limit", json!("x"))]).unwrap_err();
    assert!(err.starts_with("fs_grep() argument 'limit': "), "{err}");
    assert_eq!(
        err,
        "fs_grep() argument 'limit': expected integer, got \"x\""
    );
}

#[test]
fn a_tool_with_empty_properties_takes_no_arguments() {
    let schema = json!({"type": "object", "properties": {}});
    assert_eq!(
        bind_python_call("t", &schema, vec![], vec![]).unwrap(),
        json!({})
    );
    let err = bind_python_call("t", &schema, vec![json!(1)], vec![]).unwrap_err();
    assert_eq!(
        err.0,
        "t() takes at most 0 positional arguments but 1 were given"
    );
    let err = bind_python_call("t", &schema, vec![], vec![("x".into(), json!(1))]).unwrap_err();
    assert_eq!(err.0, "t() got an unexpected keyword argument 'x'");
}

#[test]
fn a_schema_with_no_properties_key_is_open() {
    let schema = json!({"type": "object"});
    assert_eq!(
        bind_to(&schema, vec![("x", json!(1)), ("y", json!("z"))]).unwrap(),
        json!({"x": 1, "y": "z"})
    );
    let err = bind_python_call("t", &schema, vec![json!(1)], vec![]).unwrap_err();
    assert_eq!(
        err.0,
        "t() takes at most 0 positional arguments but 1 were given"
    );
}

#[test]
fn a_required_name_that_is_not_listed_is_a_required_keyword() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "required": ["ghost", "a"]
    });
    let err = bind_to(&schema, vec![("a", json!("x"))]).unwrap_err();
    assert_eq!(err, "t() missing 1 required argument: 'ghost'");
    let bound = bind_to(&schema, vec![("ghost", json!(1)), ("a", json!("x"))]).unwrap();
    assert_eq!(bound, json!({"a": "x", "ghost": 1}));
    // It has no positional slot: only `a` does.
    let err = bind_python_call("t", &schema, vec![json!("x"), json!(1)], vec![]).unwrap_err();
    assert_eq!(
        err.0,
        "t() takes at most 1 positional argument but 2 were given"
    );
    let err = bind_python_call("t", &schema, vec![json!("x")], vec![]).unwrap_err();
    assert_eq!(err.0, "t() missing 1 required argument: 'ghost'");
}

#[test]
fn a_required_unlisted_name_is_kept_when_none() {
    let schema = json!({"type": "object", "properties": {}, "required": ["ghost"]});
    let bound = bind_to(&schema, vec![("ghost", Value::Null)]).unwrap();
    assert_eq!(bound, json!({"ghost": null}));
}

#[test]
fn positional_order_follows_properties_not_required() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
        "required": ["b", "a"]
    });
    let bound = bind_python_call("t", &schema, vec![json!("A"), json!("B")], vec![]).unwrap();
    assert_eq!(bound, json!({"a": "A", "b": "B"}));
}

#[test]
fn none_for_a_nullable_required_property_stays_null() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": ["string", "null"]}},
        "required": ["a"]
    });
    let bound = bind_python_call("t", &schema, vec![Value::Null], vec![]).unwrap();
    assert_eq!(bound, json!({"a": null}));
}

#[test]
fn none_for_a_non_nullable_required_property_is_the_validators_error() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "required": ["a"]
    });
    let err = bind_python_call("t", &schema, vec![Value::Null], vec![]).unwrap_err();
    assert_eq!(err.0, "t() argument 'a': expected string, got null");
}

#[test]
fn a_missing_required_property_beats_an_allowed_extra_keyword() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "required": ["a"],
        "additionalProperties": true
    });
    let err = bind_to(&schema, vec![("extra", json!(1))]).unwrap_err();
    assert_eq!(err, "t() missing 1 required argument: 'a'");
}

#[test]
fn structured_content_is_returned_whole() {
    let output =
        json!({"content": [{"type": "text", "text": "hi"}], "structuredContent": {"a": 1}});
    assert_eq!(python_value(&output), json!({"a": 1}));
}

#[test]
fn a_plain_object_without_content_is_returned_as_it_is() {
    let output = json!({"env": {"HTTPS_PROXY": "http://p"}});
    assert_eq!(python_value(&output), output);
    assert_eq!(python_value(&json!({})), json!({}));
}

#[test]
fn structured_content_of_any_type_is_returned_whole() {
    let output = json!({"content": [], "structuredContent": [1, 2]});
    assert_eq!(python_value(&output), json!([1, 2]));
}

#[test]
fn text_blocks_are_returned_as_the_commands_text() {
    let output =
        json!({"content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]});
    assert_eq!(python_value(&output), json!("one\ntwo\n"));
}

#[test]
fn user_audience_blocks_are_left_out() {
    let output = json!({"content": [
        {"type": "text", "text": "summary", "annotations": {"audience": ["user"]}},
        {"type": "text", "text": "data", "annotations": {"audience": ["assistant"]}}
    ]});
    assert_eq!(python_value(&output), json!("data\n"));
}

#[test]
fn an_empty_result_is_an_empty_string() {
    assert_eq!(python_value(&json!({"content": []})), json!(""));
}

fn bind_to(schema: &Value, kwargs: Vec<(&str, Value)>) -> Result<Value, String> {
    let kwargs = kwargs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    bind_python_call("t", schema, vec![], kwargs).map_err(|e| e.0)
}

#[test]
fn free_form_keywords_pass_through_when_additional_properties_allows_them() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}, "b": {"type": "string"}},
        "additionalProperties": true
    });
    let bound = bind_to(
        &schema,
        vec![("z", json!(1)), ("b", json!("B")), ("y", json!("Y"))],
    );
    let bound = bound.unwrap();
    assert_eq!(bound, json!({"b": "B", "z": 1, "y": "Y"}));
    let keys: Vec<&String> = bound.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["b", "z", "y"]);
}

#[test]
fn a_free_form_keyword_is_checked_against_the_additional_properties_schema() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "additionalProperties": {"type": "string"}
    });
    assert_eq!(
        bind_to(&schema, vec![("z", json!("Z")), ("a", json!("A"))]).unwrap(),
        json!({"a": "A", "z": "Z"})
    );
    assert_eq!(
        bind_to(&schema, vec![("z", json!(1))]).unwrap_err(),
        "t() argument 'z': expected string, got 1"
    );
}

#[test]
fn explicit_additional_properties_false_closes_a_schema_with_no_properties_key() {
    let schema = json!({"type": "object", "additionalProperties": false});
    assert_eq!(bind_to(&schema, vec![]).unwrap(), json!({}));
    assert_eq!(
        bind_to(&schema, vec![("x", json!(1))]).unwrap_err(),
        "t() got an unexpected keyword argument 'x'"
    );
}

#[test]
fn none_for_a_nullable_optional_property_stays_null() {
    for nullable in [
        json!({"type": ["string", "null"]}),
        json!({"anyOf": [{"type": "string"}, {"type": "null"}]}),
        json!({"type": "string", "nullable": true}),
    ] {
        let schema = json!({
            "type": "object",
            "properties": {"a": nullable, "b": {"type": "string"}}
        });
        let bound = bind_to(&schema, vec![("a", Value::Null), ("b", Value::Null)]);
        assert_eq!(bound.unwrap(), json!({"a": null}));
    }
}

#[test]
fn none_for_a_free_form_keyword_stays_only_when_its_schema_accepts_null() {
    let nullable = json!({"type": "object", "additionalProperties": {"type": ["string", "null"]}});
    assert_eq!(
        bind_to(&nullable, vec![("x", Value::Null)]).unwrap(),
        json!({"x": null})
    );
    for extra in [json!(true), json!({"type": "string"})] {
        let schema = json!({"type": "object", "additionalProperties": extra});
        assert_eq!(
            bind_to(&schema, vec![("x", Value::Null)]).unwrap(),
            json!({})
        );
    }
}

#[test]
fn a_schema_with_no_properties_takes_free_form_keywords() {
    let schema = json!({"type": "object", "additionalProperties": true});
    assert_eq!(
        bind_to(&schema, vec![("x", json!(1))]).unwrap(),
        json!({"x": 1})
    );
}

#[test]
fn unknown_keywords_stay_errors_when_additional_properties_is_false_or_absent() {
    for schema in [
        json!({"type": "object", "properties": {"a": {}}, "additionalProperties": false}),
        json!({"type": "object", "properties": {"a": {}}}),
    ] {
        assert_eq!(
            bind_to(&schema, vec![("x", json!(1))]).unwrap_err(),
            "t() got an unexpected keyword argument 'x'"
        );
    }
}
