use serde_json::json;

use super::*;

fn reader() -> ToolSpec {
    ToolSpec {
        name: "files_fs_read".into(),
        description: "Read source files. More detail stays in --help.".into(),
        schema: json!({"type":"object", "properties":{
            "path":{"type":"string"}, "head_lines":{"type":"integer"}
        }, "required":["path"]}),
    }
}

fn creator(encoding: bool) -> ToolSpec {
    let mut tool = ToolSpec {
        name: "media_attachment_create".into(),
        description: "Create an attachment.".into(),
        schema: json!({"type":"object", "properties":{
            "content":{"type":"string"}, "mime_type":{"type":"string"}
        }, "required":["content", "mime_type"]}),
    };
    if encoding {
        tool.schema["properties"]["encoding"] = json!({"type":"string", "enum":["utf8", "base64"]});
    }
    tool
}

fn fixture() -> Vec<ToolSpec> {
    vec![
        creator(true),
        reader(),
        ToolSpec {
            name: "search".into(),
            description: "Search indexed records.\nMore details.".into(),
            schema: json!({"type":"object", "properties":{
            "query":{"type":"string"}, "includeArchived":{"type":"boolean"},
            "tags":{"type":"array", "items":{"type":"string"}}, "filter":{"type":"object"}
        }, "required":["query"]}),
        },
    ]
}

#[test]
fn description_snapshot_and_token_estimate() {
    let tools = fixture();
    let description = bash_tool_description(&tools, 8, 1000);
    let synopsis_chars: usize = tools
        .iter()
        .map(|tool| {
            synopsis(&tool.name, &tool.schema, &tool.description)
                .chars()
                .count()
                + 3
        })
        .sum();
    // Rough characters/4 budget signal, not a provider tokenizer measurement.
    let fixed_tokens = (description.chars().count() - synopsis_chars).div_ceil(4);
    eprintln!("bash description estimate: {fixed_tokens} fixed tokens + {} synopsis tokens (characters/4)", synopsis_chars.div_ceil(4));
    assert!(
        fixed_tokens < 1500,
        "fixed guide grew to ~{fixed_tokens} tokens"
    );
    insta::assert_snapshot!("bash_description", description);
}

#[test]
fn description_is_catalog_order_independent_and_uses_shared_synopses() {
    let mut tools = fixture();
    let expected = bash_tool_description(&tools, 8, 1000);
    tools.reverse();
    assert_eq!(bash_tool_description(&tools, 8, 1000), expected);
    for tool in tools {
        let line = format!(
            "- {}",
            synopsis(&tool.name, &tool.schema, &tool.description)
        );
        assert_eq!(expected.lines().filter(|&item| item == line).count(), 1);
    }
    assert!(!expected.contains("More detail"));
    assert_eq!(expected.matches("Worked pipeline:").count(), 1);
    assert!(expected.contains("files_fs_read --path src/lib.rs -o text |"));
    assert!(expected.contains("Inner tools: 8 concurrent, 1000 admitted calls"));
    assert!(
        bash_tool_description(&[], 2, 7).contains("Inner tools: 2 concurrent, 7 admitted calls")
    );
}

#[test]
fn builtin_list_matches_backend_with_network_commands_filtered() {
    let mut names = BashKitBackend.builtin_names();
    names.sort();
    names.dedup();
    assert_eq!(builtin_names(), names.join(", "));
    for name in [
        "python", "python3", "cat", "base64", "xargs", "printf", "rg", "head",
    ] {
        assert!(names.iter().any(|item| item == name), "missing {name}");
    }
    for name in ["curl", "wget"] {
        assert!(!names.iter().any(|item| item == name));
    }
}

#[test]
fn attachment_idiom_requires_compatible_encoding_schema() {
    assert!(examples::attachment_idiom(&[&creator(false)]).is_none());
    let mut tool = creator(true);
    assert_eq!(examples::attachment_idiom(&[&tool]).unwrap(),
        "media_attachment_create --mime-type image/png --encoding base64 --content \"$(base64 -w0 /tmp/x.png)\"");
    tool.schema["properties"]["encoding"]["enum"] = json!(["utf8"]);
    assert!(examples::attachment_idiom(&[&tool]).is_none());
    tool = creator(true);
    tool.schema["properties"]["mime_type"]["enum"] = json!(["text/plain"]);
    assert!(examples::attachment_idiom(&[&tool]).is_none());
    tool = creator(true);
    tool.schema["properties"]["title"] = json!({"type":"string"});
    tool.schema["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("title"));
    assert!(examples::attachment_idiom(&[&tool]).is_none());
    tool = creator(true);
    tool.name = "not_a_creator".into();
    assert!(examples::attachment_idiom(&[&tool]).is_none());
    tool = creator(true);
    tool.schema["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("unlistedRequired"));
    assert!(examples::attachment_idiom(&[&tool]).is_none());
}

#[test]
fn pipeline_falls_back_without_a_compatible_reader() {
    assert!(examples::pipeline(&[]).contains("Generic illustration, not an extra installed tool"));
    let mut tool = reader();
    tool.schema["properties"]["path"]["type"] = json!("integer");
    assert!(examples::pipeline(&[&tool]).contains("Generic illustration"));
    tool = reader();
    tool.schema["properties"]["token"] = json!({"type":"string"});
    tool.schema["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("token"));
    assert!(examples::pipeline(&[&tool]).contains("Generic illustration"));
    // Reader whose path enum excludes src/lib.rs falls back to generic illustration
    tool = reader();
    tool.schema["properties"]["path"]["enum"] = json!(["Cargo.toml", "README.md"]);
    assert!(examples::pipeline(&[&tool]).contains("Generic illustration"));
    // Reader whose path enum includes src/lib.rs is selected
    tool = reader();
    tool.schema["properties"]["path"]["enum"] = json!(["src/lib.rs", "README.md"]);
    assert!(examples::pipeline(&[&tool]).contains("files_fs_read --path src/lib.rs -o text |"));
    // Reader with an unlisted required property (not present in properties map) falls back
    tool = reader();
    tool.schema["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("unlisted_field"));
    assert!(examples::pipeline(&[&tool]).contains("Generic illustration"));
    // Normalizing an unlisted key must not alias an existing property's flag.
    tool.schema["required"] = json!(["path", "Path"]);
    assert!(examples::pipeline(&[&tool]).contains("Generic illustration"));
}

mod execution;
