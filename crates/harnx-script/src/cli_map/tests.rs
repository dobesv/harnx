use serde_json::{json, Value};

use super::{parse_args, split_output_args, OutputFormat, UsageError};

fn argv(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}

fn schema() -> Value {
    json!({"type":"object", "properties": {
        "filePath":{"type":"string"}, "max_output_bytes":{"type":"integer"},
        "ratio":{"type":"number"}, "enabled":{"type":"boolean"},
        "tags":{"type":"array", "items":{"type":"string"}},
        "counts":{"type":"array", "items":{"type":"integer"}},
        "ratios":{"type":"array", "items":{"type":"number"}},
        "toggles":{"type":"array", "items":{"type":"boolean"}},
        "settings":{"type":"object"},
        "records":{"type":"array", "items":{"type":"object"}},
        "matrix":{"type":"array", "items":{"type":"array", "items":{"type":"integer"}}},
        "nullable":{"type":["string","null"]},
        "choice":{"anyOf":[{"type":"string"},{"type":"null"}]},
        "reference":{"$ref":"#/$defs/thing"},
        "defaultValue":{"type":"integer", "default":42},
        "mode":{"type":"string", "enum":["json","text"]}
    }})
}

#[test]
fn argument_rules_table() {
    let cases = [
        ("empty and defaults not injected", vec![], None, json!({})),
        (
            "camelCase key",
            vec!["--file-path", "two words"],
            None,
            json!({"filePath":"two words"}),
        ),
        (
            "snake_case key and equals",
            vec!["--max-output-bytes=42"],
            None,
            json!({"max_output_bytes":42}),
        ),
        (
            "empty string",
            vec!["--file-path="],
            None,
            json!({"filePath":""}),
        ),
        (
            "string isn't JSON decoded",
            vec!["--file-path", "null"],
            None,
            json!({"filePath":"null"}),
        ),
        (
            "equals in value",
            vec!["--file-path=a=b"],
            None,
            json!({"filePath":"a=b"}),
        ),
        (
            "negative integer",
            vec!["--max-output-bytes", "-4"],
            None,
            json!({"max_output_bytes":-4}),
        ),
        (
            "largest u64",
            vec!["--max-output-bytes", "18446744073709551615"],
            None,
            json!({"max_output_bytes":u64::MAX}),
        ),
        (
            "smallest i64",
            vec!["--max-output-bytes", "-9223372036854775808"],
            None,
            json!({"max_output_bytes":i64::MIN}),
        ),
        (
            "number decimal",
            vec!["--ratio", "-1.25"],
            None,
            json!({"ratio":-1.25}),
        ),
        (
            "number exponent",
            vec!["--ratio=1e3"],
            None,
            json!({"ratio":1000.0}),
        ),
        (
            "number integer",
            vec!["--ratio", "2"],
            None,
            json!({"ratio":2}),
        ),
        (
            "boolean on",
            vec!["--enabled"],
            None,
            json!({"enabled":true}),
        ),
        (
            "boolean off",
            vec!["--no-enabled"],
            None,
            json!({"enabled":false}),
        ),
        (
            "last switch wins",
            vec!["--enabled", "--no-enabled", "--enabled"],
            None,
            json!({"enabled":true}),
        ),
        (
            "last scalar wins",
            vec!["--ratio=1", "--ratio=2"],
            None,
            json!({"ratio":2}),
        ),
        (
            "repeated strings",
            vec!["--tags=a", "--tags", "b"],
            None,
            json!({"tags":["a","b"]}),
        ),
        (
            "repeated integers",
            vec!["--counts=1", "--counts=-2"],
            None,
            json!({"counts":[1,-2]}),
        ),
        (
            "repeated numbers",
            vec!["--ratios", "0.5", "--ratios=2"],
            None,
            json!({"ratios":[0.5,2]}),
        ),
        (
            "repeated booleans need values",
            vec!["--toggles=true", "--toggles", "false"],
            None,
            json!({"toggles":[true,false]}),
        ),
        (
            "object JSON",
            vec!["--settings", "{\"a\":{\"b\":1}}"],
            None,
            json!({"settings":{"a":{"b":1}}}),
        ),
        (
            "complex array JSON",
            vec!["--records=[{\"a\":1}]"],
            None,
            json!({"records":[{"a":1}]}),
        ),
        (
            "nested arrays JSON",
            vec!["--matrix", "[[1,2],[3]]"],
            None,
            json!({"matrix":[[1,2],[3]]}),
        ),
        (
            "nullable null",
            vec!["--nullable=null"],
            None,
            json!({"nullable":null}),
        ),
        (
            "nullable string JSON",
            vec!["--nullable=\"value\""],
            None,
            json!({"nullable":"value"}),
        ),
        (
            "anyOf null",
            vec!["--choice=null"],
            None,
            json!({"choice":null}),
        ),
        (
            "ref JSON fallback",
            vec!["--reference={\"value\":1}"],
            None,
            json!({"reference":{"value":1}}),
        ),
        (
            "JSON base",
            vec!["--json", "{\"filePath\":\"base\",\"extra\":true}"],
            None,
            json!({"filePath":"base","extra":true}),
        ),
        (
            "JSON base from stdin",
            vec!["--json", "-"],
            Some("{\"filePath\":\"stdin\"}"),
            json!({"filePath":"stdin"}),
        ),
        (
            "stdin with equals",
            vec!["--json=-", "--file-path=override"],
            Some("{\"filePath\":\"stdin\"}"),
            json!({"filePath":"override"}),
        ),
        ("stdin ignored", vec![], Some("not JSON"), json!({})),
        (
            "stdin ignored with literal JSON",
            vec!["--json={}"],
            Some("not JSON"),
            json!({}),
        ),
        (
            "flag after JSON wins",
            vec!["--json={\"filePath\":\"base\"}", "--file-path=flag"],
            None,
            json!({"filePath":"flag"}),
        ),
        (
            "flag before JSON wins",
            vec!["--file-path=flag", "--json={\"filePath\":\"base\"}"],
            None,
            json!({"filePath":"flag"}),
        ),
        (
            "arrays replace base",
            vec!["--tags=flag", "--json={\"tags\":[\"base\"]}", "--tags=more"],
            None,
            json!({"tags":["flag","more"]}),
        ),
        (
            "invalid base value can be overridden",
            vec!["--json={\"ratio\":\"bad\"}", "--ratio=2"],
            None,
            json!({"ratio":2}),
        ),
        (
            "last JSON replaces base object",
            vec!["--json={\"ratio\":1}", "--json={\"filePath\":\"last\"}"],
            None,
            json!({"filePath":"last"}),
        ),
        (
            "base can set empty array",
            vec!["--json={\"tags\":[]}"],
            None,
            json!({"tags":[]}),
        ),
        (
            "enum accepted",
            vec!["--mode=text"],
            None,
            json!({"mode":"text"}),
        ),
        (
            "end options with no positionals",
            vec!["--file-path=x", "--"],
            None,
            json!({"filePath":"x"}),
        ),
        (
            "option-looking value via equals",
            vec!["--file-path=--enabled"],
            None,
            json!({"filePath":"--enabled"}),
        ),
        (
            "short-looking value literal",
            vec!["--file-path", "-o"],
            None,
            json!({"filePath":"-o"}),
        ),
    ];
    for (name, args, stdin, expected) in cases {
        let actual =
            parse_args(&schema(), &argv(&args), stdin).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(actual, expected, "{name}");
    }
}

#[test]
fn usage_errors_table() {
    let cases = [
        (vec!["--file-path"], None, "--file-path requires a value"),
        (
            vec!["--file-path", "--enabled"],
            None,
            "--file-path requires a value",
        ),
        (
            vec!["--max-output-bytes=1.0"],
            None,
            "expected a signed/unsigned 64-bit integer",
        ),
        (
            vec!["--max-output-bytes=1e3"],
            None,
            "expected a signed/unsigned 64-bit integer",
        ),
        (
            vec!["--max-output-bytes=18446744073709551616"],
            None,
            "expected a signed/unsigned 64-bit integer",
        ),
        (vec!["--max-output-bytes=word"], None, "expected integer"),
        (vec!["--ratio=NaN"], None, "expected number"),
        (vec!["--ratio=Infinity"], None, "expected number"),
        (vec!["--ratio=1e999"], None, "expected number"),
        (vec!["--ratio=+1"], None, "expected number"),
        (vec!["--ratio=01"], None, "expected number"),
        (vec!["--enabled=false"], None, "is a switch"),
        (vec!["--no-enabled=true"], None, "is a switch"),
        (vec!["--enabled", "false"], None, "unexpected argument"),
        (vec!["--toggles"], None, "requires a value"),
        (
            vec!["--toggles=yes"],
            None,
            "expected boolean true or false",
        ),
        (vec!["--counts=x"], None, "expected integer"),
        (vec!["--no-ratio"], None, "unknown flag"),
        (vec!["--settings=bad"], None, "expected JSON"),
        (vec!["--settings=[]"], None, "expected a JSON object"),
        (vec!["--records={}"], None, "expected a JSON array"),
        (vec!["--records=[1]"], None, "expected a JSON object"),
        (vec!["--matrix=[[\"bad\"]]"], None, "expected integer"),
        (vec!["--nullable=plain"], None, "expected JSON"),
        (vec!["--json=[]"], None, "requires a JSON object"),
        (vec!["--json=null"], None, "requires a JSON object"),
        (vec!["--json=broken"], None, "invalid --json object"),
        (vec!["--json"], None, "requires a value"),
        (vec!["--json", "-"], None, "requires stdin"),
        (vec!["--json", "-"], Some("bad"), "invalid --json object"),
        (
            vec!["--json={\"enabled\":\"false\"}"],
            None,
            "expected boolean",
        ),
        (
            vec!["--json={\"max_output_bytes\":1.5}"],
            None,
            "expected integer",
        ),
        (
            vec!["--json={\"counts\":[\"bad\"]}"],
            None,
            "expected integer",
        ),
        (
            vec!["--mode=xml"],
            None,
            "expected one of [\"json\",\"text\"]",
        ),
        (vec!["--json={\"mode\":\"xml\"}"], None, "expected one of"),
        (vec!["--file-pat=x"], None, "did you mean '--file-path'?"),
        (
            vec!["--zzzzzzzzzzzz=x"],
            None,
            "unknown flag '--zzzzzzzzzzzz'",
        ),
        (vec!["--jsno={}"], None, "did you mean '--json'?"),
        (vec!["--", "--enabled"], None, "no positional arguments"),
        (vec!["positional"], None, "unexpected argument"),
        (vec!["-x"], None, "unexpected argument"),
        (vec!["-o", "json"], None, "call split_output_args"),
    ];
    for (args, stdin, expected) in cases {
        let error = parse_args(&schema(), &argv(&args), stdin).expect_err(&format!("{args:?}"));
        assert!(
            error.to_string().contains(expected),
            "{args:?}: {error}; expected {expected}"
        );
    }
    let error = parse_args(&schema(), &argv(&["--zzzzzzzzzzzz=x"]), None).unwrap_err();
    assert!(!error.message.contains("did you mean"));
}

#[test]
fn required_fields_list_original_flags_and_accept_false_or_null() {
    let schema = json!({"properties": {"filePath":{"type":"string"},"dry_run":{"type":"boolean"},"maybe":{"type":["string","null"]}}, "required":["filePath","dry_run","maybe"]});
    let error = parse_args(&schema, &[], None).unwrap_err();
    assert!(error.message.contains("--file-path, --dry-run, --maybe"));
    assert_eq!(
        parse_args(
            &schema,
            &argv(&["--file-path=x", "--no-dry-run", "--maybe=null"]),
            None
        )
        .unwrap(),
        json!({"filePath":"x","dry_run":false,"maybe":null})
    );
    assert!(parse_args(
        &schema,
        &argv(&["--json={\"filePath\":\"x\",\"dry_run\":false,\"maybe\":null}"]),
        None
    )
    .is_ok());
}

#[test]
fn kebab_lookup_round_trips_original_keys() {
    for (key, flag) in [
        ("snake_case", "snake-case"),
        ("camelCase", "camel-case"),
        ("HTTPServerURL", "http-server-url"),
        ("already-kebab", "already-kebab"),
        ("UTF8String", "utf8-string"),
        ("caféMode", "café-mode"),
    ] {
        let mut schema = json!({"properties":{}});
        schema["properties"][key] = json!({"type":"string"});
        let parsed = parse_args(&schema, &[format!("--{flag}=value")], None).unwrap();
        assert_eq!(
            parsed.as_object().unwrap().keys().collect::<Vec<_>>(),
            [key]
        );
        assert_eq!(parsed[key], "value");
    }
}

#[test]
fn complex_schema_fallback_table() {
    for (shape, value) in [
        (json!({"$ref":"#/$defs/item"}), json!({"nested":1})),
        (
            json!({"type":"string", "$ref":"external.json"}),
            json!("text"),
        ),
        (
            json!({"anyOf":[{"type":"string"},{"type":"null"}]}),
            Value::Null,
        ),
        (
            json!({"oneOf":[{"type":"number"},{"type":"object"}]}),
            json!(42),
        ),
        (json!({"allOf":[{"type":"object"}]}), json!({})),
        (json!({"type":["boolean","null"]}), json!(false)),
        (json!({"type":"string", "nullable":true}), Value::Null),
        (
            json!({"type":"array", "items":{"type":["string","null"]}}),
            json!(["a", null]),
        ),
        (
            json!({"type":"array", "items":{"$ref":"#/$defs/item"}}),
            json!([{"x":1}]),
        ),
        (
            json!({"type":"array", "items":{"type":"string"}, "anyOf":[{}]}),
            json!(["a", "b"]),
        ),
        (
            json!({"type":"array", "items":{"type":"string"}, "not":{"maxItems":0}}),
            json!(["a"]),
        ),
        (
            json!({"type":"array", "items":{"type":"string"}, "if":{}, "then":{}}),
            json!(["a"]),
        ),
        (
            json!({"type":"array", "items":{"type":"string"}, "prefixItems":[{"type":"number"}]}),
            json!([1, "a"]),
        ),
        (json!({}), json!("untyped")),
        (json!(true), json!("untyped")),
    ] {
        let schema = json!({"properties":{"value":shape}});
        assert_eq!(
            parse_args(&schema, &[format!("--value={value}")], None).unwrap(),
            json!({"value":value}),
            "{schema}"
        );
        assert!(parse_args(&schema, &argv(&["--value=bare"]), None).is_err());
        assert!(crate::help::synopsis("tool", &schema, "").contains("--value <json>"));
    }
    // Root-level references/unions still accept a base object without panicking.
    for schema in [
        json!({"$ref":"#/$defs/Tool"}),
        json!({"anyOf":[]}),
        json!(true),
    ] {
        assert_eq!(
            parse_args(&schema, &argv(&["--json={\"rawKey\":42}"]), None).unwrap(),
            json!({"rawKey":42})
        );
    }
}

#[test]
fn enums_include_array_items_and_numeric_json_equality() {
    let schema = json!({"properties": {
        "values":{"type":"array", "items":{"type":"string","enum":["a","b"]}},
        "number":{"type":"number", "enum":[1.0,2]},
        "untyped":{"enum":[null,"a"]}
    }});
    assert!(parse_args(
        &schema,
        &argv(&["--values=a", "--values=b", "--number=1", "--untyped=null"]),
        None
    )
    .is_ok());
    assert!(parse_args(&schema, &argv(&["--number=2.0"]), None).is_ok());
    for args in [
        vec!["--values=c"],
        vec!["--json={\"values\":[\"c\"]}"],
        vec!["--number=1.5"],
        vec!["--untyped=42"],
    ] {
        assert!(parse_args(&schema, &argv(&args), None)
            .unwrap_err()
            .message
            .contains("expected one of"));
    }
    let precise = json!({"properties":{"number":{"type":"number","enum":[9007199254740993_u64]}}});
    assert!(parse_args(&precise, &argv(&["--number=9007199254740992.0"]), None).is_err());
}

#[test]
fn collisions_and_reserved_json_have_an_escape_hatch() {
    for properties in [
        json!({"filePath":{"type":"string"},"file_path":{"type":"string"}}),
        json!({"flag":{"type":"boolean"},"no_flag":{"type":"string"}}),
    ] {
        let schema = json!({"properties":properties});
        assert!(parse_args(&schema, &argv(&["--flag"]), None)
            .unwrap_err()
            .message
            .contains("conflicting flag"));
        assert!(parse_args(&schema, &argv(&["--json={}"]), None).is_ok());
    }
    let schema = json!({"properties":{"json":{"type":"string"},"":{"type":"number"}}, "required":["json",""]});
    assert_eq!(
        parse_args(&schema, &argv(&["--json={\"json\":\"data\",\"\":1}"]), None).unwrap(),
        json!({"json":"data","":1})
    );
}

#[test]
fn rendering_option_split_table() {
    let schema = json!({"properties":{"path":{"type":"string"}, "flag":{"type":"boolean"}, "o":{"type":"string"}, "output":{"type":"string"}}});
    let cases = [
        (vec![], vec![], None),
        (vec!["-o", "json"], vec![], Some(OutputFormat::Json)),
        (vec!["-o=text"], vec![], Some(OutputFormat::Text)),
        (
            vec!["-o", "json", "--flag", "-o", "text"],
            vec!["--flag"],
            Some(OutputFormat::Text),
        ),
        (
            vec!["--flag", "-o", "json"],
            vec!["--flag"],
            Some(OutputFormat::Json),
        ),
        (
            vec!["--no-flag", "-o", "text"],
            vec!["--no-flag"],
            Some(OutputFormat::Text),
        ),
        (vec!["--path", "-o"], vec!["--path", "-o"], None),
        (
            vec!["--path=-o", "-o", "text"],
            vec!["--path=-o"],
            Some(OutputFormat::Text),
        ),
        (vec!["--path", "-o=json"], vec!["--path", "-o=json"], None),
        (
            vec!["--o=value", "--output", "text", "-o", "json"],
            vec!["--o=value", "--output", "text"],
            Some(OutputFormat::Json),
        ),
        (
            vec!["--json", "{\"path\":\"-o\"}", "-o", "json"],
            vec!["--json", "{\"path\":\"-o\"}"],
            Some(OutputFormat::Json),
        ),
        (vec!["--", "-o", "xml"], vec!["--", "-o", "xml"], None),
    ];
    for (args, expected, format) in cases {
        assert_eq!(
            split_output_args(&schema, &argv(&args)).unwrap(),
            (argv(&expected), format),
            "{args:?}"
        );
    }
    for args in [vec!["-o"], vec!["-o=xml"], vec!["-o", "--flag"]] {
        assert!(
            split_output_args(&schema, &argv(&args)).is_err(),
            "{args:?}"
        );
    }
    let (args, format) =
        split_output_args(&schema, &argv(&["--flag", "-o", "json", "--o=literal"])).unwrap();
    assert_eq!(format, Some(OutputFormat::Json));
    assert_eq!(
        parse_args(&schema, &args, None).unwrap(),
        json!({"flag":true,"o":"literal"})
    );
}

#[test]
fn usage_error_is_a_displayable_standard_error() {
    fn standard(error: &dyn std::error::Error) -> String {
        error.to_string()
    }
    assert_eq!(standard(&UsageError::new("usage")), "usage");
}

#[test]
fn output_split_uses_the_same_value_boundaries_as_parser() {
    for shape in [
        json!({"type":"string"}),
        json!({"type":"array", "items":{"type":"string"}}),
        json!({"type":["boolean","null"]}),
        json!({"$ref":"#/$defs/Options"}),
        json!({"type":"object"}),
    ] {
        let schema = json!({"properties":{"value":shape}});
        let args = argv(&["--value", "-o", "-o", "json"]);
        let (tool_args, format) = split_output_args(&schema, &args).unwrap();
        assert_eq!(tool_args, argv(&["--value", "-o"]), "{schema}");
        assert_eq!(format, Some(OutputFormat::Json));
    }
    let collision = json!({"properties":{"aB":{"type":"string"},"a_b":{"type":"string"}}});
    let (args, format) = split_output_args(
        &collision,
        &argv(&["--json={\"aB\":\"one\",\"a_b\":\"two\"}", "-o=json"]),
    )
    .unwrap();
    assert_eq!(format, Some(OutputFormat::Json));
    assert_eq!(
        parse_args(&collision, &args, None).unwrap(),
        json!({"aB":"one","a_b":"two"})
    );
}

#[test]
fn camel_case_round_trips_all_flag_kinds_and_json_overrides() {
    for (key, shape, args, expected) in [
        (
            "filePath",
            json!({"type":"string"}),
            vec!["--file-path=x"],
            json!("x"),
        ),
        (
            "maxCount",
            json!({"type":"integer"}),
            vec!["--max-count=3"],
            json!(3),
        ),
        (
            "scoreRatio",
            json!({"type":"number"}),
            vec!["--score-ratio=0.5"],
            json!(0.5),
        ),
        (
            "dryRun",
            json!({"type":"boolean"}),
            vec!["--no-dry-run"],
            json!(false),
        ),
        (
            "fileTags",
            json!({"type":"array","items":{"type":"string"}}),
            vec!["--file-tags=a", "--file-tags=b"],
            json!(["a", "b"]),
        ),
        (
            "toolOptions",
            json!({"type":"object"}),
            vec!["--tool-options={\"nested\":1}"],
            json!({"nested":1}),
        ),
        (
            "maybeValue",
            json!({"anyOf":[{"type":"string"},{"type":"null"}]}),
            vec!["--maybe-value=null"],
            Value::Null,
        ),
    ] {
        let mut schema = json!({"properties":{}, "required":[key]});
        schema["properties"][key] = shape;
        let parsed = parse_args(&schema, &argv(&args), None).unwrap();
        assert_eq!(parsed.as_object().unwrap().len(), 1);
        assert_eq!(parsed[key], expected);
        assert_eq!(
            parse_args(&schema, &[format!("--json={parsed}")], None).unwrap(),
            parsed
        );
        let mut base = json!({});
        base[key] = json!({"old":"replaced, not merged"});
        let mut with_base = argv(&args);
        with_base.push(format!("--json={base}"));
        assert_eq!(parse_args(&schema, &with_base, None).unwrap(), parsed);
    }
}
