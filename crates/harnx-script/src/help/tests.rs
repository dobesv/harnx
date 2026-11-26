use serde_json::{json, Value};

use super::{help, synopsis};
use crate::commands::OutputMapping;

fn schema() -> Value {
    json!({"type":"object", "required":["filePath"], "properties":{
        "filePath":{"type":"string", "description":"File to read."},
        "max_lines":{"type":"integer", "default":10, "description":"Limit lines.\nZero reads none."},
        "raw":{"type":"boolean", "default":false},
        "mode":{"type":"string", "enum":["json","text"], "default":"text"},
        "tags":{"type":"array", "items":{"type":"string", "enum":["a","b"]}},
        "options":{"type":"object"}
    }})
}

#[test]
fn synopsis_is_one_line_required_first_with_original_key_lookup() {
    assert_eq!(synopsis("fs_read", &schema(), "Read a file.\nReturns its contents."),
        "fs_read --file-path <string> [--max-lines <integer>] [--mode <string>] [--options <json>] [--raw|--no-raw] [--tags <string>]... [--json <object>]  # Read a file.");
}

#[test]
fn help_has_man_page_sections_and_property_metadata() {
    let actual = help(
        "fs_read",
        &schema(),
        "Read a file.\nReturns its contents.",
        None,
        None,
    );
    let expected = concat!(
        "NAME\n    fs_read - Read a file.\n\n",
        "SYNOPSIS\n    fs_read --file-path <string> [--max-lines <integer>] [--mode <string>] [--options <json>] [--raw|--no-raw] [--tags <string>]... [--json <object>]\n",
        "    PYTHON  tools.fs_read(filePath, *, max_lines=None, mode=None, options=None, raw=None, tags=None)\n\n",
        "DESCRIPTION\n    Read a file.\n    Returns its contents.\n\n",
        "OPTIONS\n",
        "    --file-path <string>\n        type: string; required\n        File to read.\n",
        "    --max-lines <integer>\n        type: integer; optional; default: 10\n        Limit lines.\n        Zero reads none.\n",
        "    --mode <string>\n        type: string; optional; default: \"text\"; enum: [\"json\",\"text\"]\n",
        "    --options <json>\n        type: json; optional\n",
        "    --raw / --no-raw\n        type: boolean; optional; default: false\n",
        "    --tags <string>\n        type: string[]; optional; repeat flag for each item; item enum: [\"a\",\"b\"]\n",
        "    --json <object> | --json -\n        Base object; '-' reads stdin. Flags override base keys in any order.\n",
        "    -o json|text\n        Output rendering (handled by the command wrapper, not sent to the tool).\n",
        "    --help\n        Show tool help (handled by the command wrapper).\n",
        "    --\n        End options. Positional arguments aren't supported.\n",
    );
    assert_eq!(actual, expected);
}

#[test]
fn first_sentence_and_whitespace_table() {
    for (description, expected) in [
        ("First sentence. Second sentence.", "First sentence."),
        ("First! Next", "First!"),
        ("First? Next", "First?"),
        ("  A\nmultiline\tdescription  ", "A multiline description"),
        (
            "Read a.json then return it. More.",
            "Read a.json then return it.",
        ),
        ("", ""),
    ] {
        let line = synopsis("tool", &json!({}), description);
        let suffix = if expected.is_empty() {
            "".to_owned()
        } else {
            format!("  # {expected}")
        };
        assert_eq!(line, format!("tool [--json <object>]{suffix}"));
        assert!(!line.contains('\n'));
    }
}

#[test]
fn json_fallback_documentation_matches_parser_types() {
    for shape in [
        json!({"$ref":"#/$defs/Thing"}),
        json!({"anyOf":[{"type":"string"},{"type":"integer"},{"type":"null"}]}),
        json!({"type":["string","integer","null"]}),
        json!({"type":"array","items":{"type":"object"}}),
        json!({}),
    ] {
        let schema = json!({"properties":{"camelCase":shape}, "required":["camelCase"]});
        assert!(synopsis("tool", &schema, "").contains("--camel-case <json>"));
        assert!(help("tool", &schema, "", None, None).contains("type: json; required"));
    }
}

#[test]
fn deterministic_property_order_does_not_depend_on_schema_insertion_order() {
    let a = json!({"properties":{"z":{"type":"string"},"a":{"type":"integer"}}});
    let b = json!({"properties":{"a":{"type":"integer"},"z":{"type":"string"}}});
    assert_eq!(
        synopsis("tool", &a, "Summary."),
        synopsis("tool", &b, "Summary.")
    );
    assert_eq!(
        help("tool", &a, "Summary.", None, None),
        help("tool", &b, "Summary.", None, None)
    );
}

#[test]
fn collisions_and_reserved_json_document_the_escape_hatch() {
    let collision =
        json!({"properties":{"aB":{"type":"string"},"a_b":{"type":"string"}}, "required":["aB"]});
    assert_eq!(synopsis("tool", &collision, ""), "tool --json <object>");
    assert!(help("tool", &collision, "", None, None).contains("Conflicting schema flag names"));
    let reserved = json!({"properties":{"json":{"type":"string"}}, "required":["json"]});
    assert_eq!(synopsis("tool", &reserved, ""), "tool --json <object>");
    assert!(help("tool", &reserved, "", None, None).contains("(property \"json\")"));
}

#[test]
fn unlisted_required_keys_require_json_in_synopsis_and_help() {
    use crate::cli_map::parse_args;

    for properties in [
        json!({}),
        json!({"listed": {"type": "string"}}),
        json!({"aB": {"type": "string"}, "a_b": {"type": "string"}}),
    ] {
        let schema = json!({"properties": properties, "required": ["unlisted"]});
        let syntax = synopsis("tool", &schema, "");
        assert!(syntax.ends_with(" --json <object>"), "{syntax}");
        assert!(!syntax.contains("[--json"), "{syntax}");
        assert!(help("tool", &schema, "", None, None).contains(&format!("    {syntax}\n")));
        assert!(parse_args(&schema, &[], None).is_err());
        let args = vec!["--json".into(), r#"{"unlisted":1}"#.into()];
        assert_eq!(
            parse_args(&schema, &args, None).unwrap(),
            json!({"unlisted": 1})
        );
    }
}

#[test]
fn help_shows_output_section_for_a_mapped_tool() {
    let mapping = OutputMapping {
        stdout: Some("text".into()),
        stderr: Some("notes".into()),
        exit_code: Some("exit_code".into()),
        summary: Some("file text, like cat".into()),
        ..Default::default()
    };
    let text = help("fs_read", &schema(), "Read a file.", Some(&mapping), None);
    let expected = concat!(
        "\nOUTPUT\n",
        "  file text, like cat\n",
        "  stdout: the `text` field    stderr: the `notes` field    exit status: the `exit_code` field\n",
        "  -o json prints every field.\n",
    );
    assert!(text.ends_with(expected), "{text}");
}

#[test]
fn help_lists_stderr_text_before_notes() {
    let mapping = OutputMapping {
        stderr_text: Some("err".into()),
        stderr: Some("notes".into()),
        ..Default::default()
    };
    let text = help("t", &schema(), "d", Some(&mapping), None);
    assert!(
        text.contains("stderr: the `err` field, then notes from `notes`\n"),
        "{text}"
    );
    let only_text = OutputMapping {
        stderr_text: Some("err".into()),
        ..Default::default()
    };
    let text = help("t", &schema(), "d", Some(&only_text), None);
    assert!(text.contains("  stderr: the `err` field\n"), "{text}");
}

#[test]
fn help_names_the_stderr_after_field() {
    let mapping = OutputMapping {
        stderr_text: Some("err".into()),
        stderr: Some("notes".into()),
        stderr_after: Some("diff".into()),
        ..Default::default()
    };
    let text = help("t", &schema(), "d", Some(&mapping), None);
    assert!(
        text.contains(
            "  stderr: the `err` field, then notes from `notes`, then the `diff` field\n"
        ),
        "{text}"
    );
    let only_after = OutputMapping {
        stderr_after: Some("diff".into()),
        ..Default::default()
    };
    let text = help("t", &schema(), "d", Some(&only_after), None);
    assert!(text.contains("  stderr: the `diff` field\n"), "{text}");
}

#[test]
fn help_leaves_out_output_lines_for_absent_parts() {
    let mapping = OutputMapping {
        stdout: Some("text".into()),
        ..Default::default()
    };
    let text = help("fs_read", &schema(), "Read a file.", Some(&mapping), None);
    assert!(text.ends_with("\nOUTPUT\n  stdout: the `text` field\n  -o json prints every field.\n"));
}

#[test]
fn help_has_no_output_section_without_a_mapping() {
    assert!(!help("fs_read", &schema(), "Read a file.", None, None).contains("OUTPUT"));
}

fn result_schema() -> Value {
    json!({"type":"object", "properties":{
        "exit_code":{"type":"integer", "description":"the command's exit status\nsecond line"},
        "stdout":{"type":"string"},
        "notes":{"type":"array", "items":{"type":"string"}},
        "tags":{"type":"array"},
        "meta":{"type":"object", "properties":{"a":{"type":"string"}}},
        "next":{"type":["string","null"], "description":"where to continue"},
        "either":{"type":["string","integer"]},
        "free":{},
        "lead":{"type":"string", "description":"\n  \n  after blank lines  \nlater"}
    }})
}

#[test]
fn help_lists_output_fields() {
    let text = help("t", &schema(), "d", None, Some(&result_schema()));
    let expected = concat!(
        "\nFIELDS (-o json)\n",
        "  exit_code  integer            the command's exit status\n",
        "  stdout     string\n",
        "  notes      array of string\n",
        "  tags       array\n",
        "  meta       object\n",
        "  next       string             where to continue\n",
        "  either     string or integer\n",
        "  free       any\n",
        "  lead       string             after blank lines\n",
    );
    assert!(text.ends_with(expected), "{text}");
}

#[test]
fn help_fields_follow_the_output_section() {
    let mapping = OutputMapping {
        stdout: Some("text".into()),
        ..Default::default()
    };
    let text = help("t", &schema(), "d", Some(&mapping), Some(&result_schema()));
    assert!(text.find("OUTPUT").unwrap() < text.find("FIELDS (-o json)").unwrap());
}

#[test]
fn help_without_properties_prints_no_fields_section() {
    for schema_value in [
        json!({"type":"object"}),
        json!({"type":"object", "properties":{}}),
        json!({"type":"object", "properties":[1]}),
        json!("text"),
    ] {
        let text = help("t", &schema(), "d", None, Some(&schema_value));
        assert!(!text.contains("FIELDS"), "{schema_value}");
    }
    assert!(!help("t", &schema(), "d", None, None).contains("FIELDS"));
}

fn python_line(name: &str, schema: &Value) -> String {
    let text = help(name, schema, "Do it.", None, None);
    text.lines()
        .find(|line| line.starts_with("    PYTHON  "))
        .map(str::to_owned)
        .unwrap_or_default()
}

#[test]
fn python_line_lists_required_in_schema_order_then_optional_by_name() {
    let schema = json!({"type":"object", "required":["b", "a"], "properties":{
        "z":{"type":"string"}, "a":{"type":"string"}, "m":{"type":"integer"}, "b":{"type":"string"}
    }});
    assert_eq!(
        python_line("t", &schema),
        "    PYTHON  tools.t(a, b, *, m=None, z=None)"
    );
}

#[test]
fn python_line_has_no_star_without_optional_properties_and_is_absent_without_properties() {
    let only_required =
        json!({"type":"object", "required":["p"], "properties":{"p":{"type":"string"}}});
    assert_eq!(python_line("t", &only_required), "    PYTHON  tools.t(p)");
    assert_eq!(python_line("t", &json!({"type":"object"})), "");
    assert_eq!(
        python_line("t", &json!({"type":"object", "properties":{}})),
        ""
    );
}

#[test]
fn python_line_reaches_odd_tool_names_with_getattr() {
    let schema = json!({"type":"object", "properties":{"p":{"type":"string"}}});
    assert_eq!(
        python_line("odd-name", &schema),
        "    PYTHON  getattr(tools, \"odd-name\")(*, p=None)"
    );
    assert_eq!(
        python_line("class", &schema),
        "    PYTHON  getattr(tools, \"class\")(*, p=None)"
    );
}

#[test]
fn python_line_leaves_out_names_python_cannot_spell_and_ends_with_a_dict_splat() {
    let schema = json!({"type":"object", "required":["from"], "properties":{
        "from":{"type":"string"}, "max-depth":{"type":"integer"}, "limit":{"type":"integer"}
    }});
    assert_eq!(
        python_line("t", &schema),
        "    PYTHON  tools.t(*, limit=None, **{...})"
    );
    let only_odd = json!({"type":"object", "properties":{"from":{"type":"string"}}});
    assert_eq!(python_line("t", &only_odd), "    PYTHON  tools.t(**{...})");
}

#[test]
fn python_line_stops_positionals_at_the_first_unspellable_required_property() {
    let schema = json!({"type":"object", "required":["a", "from", "b"], "properties":{
        "a":{"type":"string"}, "from":{"type":"string"}, "b":{"type":"string"}, "c":{"type":"string"}
    }});
    assert_eq!(
        python_line("t", &schema),
        "    PYTHON  tools.t(a, *, b=..., c=None, **{...})"
    );
}

#[test]
fn python_line_with_an_unspellable_required_property_first_has_no_positionals() {
    let schema = json!({"type":"object", "required":["from", "b"], "properties":{
        "from":{"type":"string"}, "b":{"type":"string"}
    }});
    assert_eq!(
        python_line("t", &schema),
        "    PYTHON  tools.t(*, b=..., **{...})"
    );
}
