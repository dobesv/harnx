use serde_json::{json, Value};

use super::{help, synopsis};

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
    let actual = help("fs_read", &schema(), "Read a file.\nReturns its contents.");
    let expected = concat!(
        "NAME\n    fs_read - Read a file.\n\n",
        "SYNOPSIS\n    fs_read --file-path <string> [--max-lines <integer>] [--mode <string>] [--options <json>] [--raw|--no-raw] [--tags <string>]... [--json <object>]\n\n",
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
        json!({"anyOf":[{"type":"string"},{"type":"null"}]}),
        json!({"type":["string","null"]}),
        json!({"type":"array","items":{"type":"object"}}),
        json!({}),
    ] {
        let schema = json!({"properties":{"camelCase":shape}, "required":["camelCase"]});
        assert!(synopsis("tool", &schema, "").contains("--camel-case <json>"));
        assert!(help("tool", &schema, "").contains("type: json; required"));
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
    assert_eq!(help("tool", &a, "Summary."), help("tool", &b, "Summary."));
}

#[test]
fn collisions_and_reserved_json_document_the_escape_hatch() {
    let collision =
        json!({"properties":{"aB":{"type":"string"},"a_b":{"type":"string"}}, "required":["aB"]});
    assert_eq!(synopsis("tool", &collision, ""), "tool --json <object>");
    assert!(help("tool", &collision, "").contains("Conflicting schema flag names"));
    let reserved = json!({"properties":{"json":{"type":"string"}}, "required":["json"]});
    assert_eq!(synopsis("tool", &reserved, ""), "tool --json <object>");
    assert!(help("tool", &reserved, "").contains("(property \"json\")"));
}
