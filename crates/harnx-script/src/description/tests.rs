use serde_json::json;

use super::*;

fn reader() -> ToolSpec {
    ToolSpec {
        output: None,
        output_schema: None,
        name: "files_fs_read".into(),
        description: "Read source files. More detail stays in --help.".into(),
        schema: json!({"type":"object", "properties":{
            "path":{"type":"string"}, "head_lines":{"type":"integer"}
        }, "required":["path"]}),
    }
}

fn creator(encoding: bool) -> ToolSpec {
    let mut tool = ToolSpec {
        output: None,
        output_schema: None,
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
            output: None,
            output_schema: None,
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
    for name in ["curl", "wget", "http"] {
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

#[test]
fn description_says_when_to_script_and_how_flags_and_output_behave() {
    let text = bash_tool_description(&fixture(), 8, 1000);
    for expected in [
        "Worth it for loops, many calls, or filtering a large result",
        "call the tool directly",
        "optional ones too",
    ] {
        assert!(text.contains(expected), "missing {expected:?}");
    }
}

#[test]
fn description_explains_idiomatic_output() {
    let text = bash_tool_description(&fixture(), 8, 1000);
    for expected in [
        "exit 1 when nothing matches",
        "Notes about cut-off results go to stderr",
        "-o json prints the tool's fields; help <tool> lists them under FIELDS when the tool declares them",
        "line numbers",
        "a tool's own error status",
    ] {
        assert!(text.contains(expected), "missing {expected:?}");
    }
}

#[test]
fn pipeline_with_only_a_reader_asks_for_its_text() {
    let text = bash_tool_description(&[reader()], 8, 1000);
    assert!(
        text.contains("files_fs_read --path src/lib.rs -o text | grep -n 'TODO' | head -3"),
        "{text}"
    );
}

fn shell_tool(name: &str, with_diff: bool) -> ToolSpec {
    let mut properties = json!({"command": {"type": "string"}});
    if with_diff {
        properties["diff"] =
            json!({"type": "string", "enum": ["none", "summary", "files", "full"]});
    }
    ToolSpec {
        output: Some(crate::commands::OutputMapping::default()),
        output_schema: None,
        name: name.into(),
        description: "Run a command.".into(),
        schema: json!({"type": "object", "properties": properties, "required": ["command"]}),
    }
}

const CHANGE_REPORT: &str = "- A tool that changes files reports the change on stderr in one line: its size and the git diff command that shows it (pipe that command into git apply -R to undo the change). --diff full prints the diff, --diff files the changed files, --diff none skips the snapshot (nothing to roll back to).";

#[test]
fn change_report_bullet_names_the_agents_own_bash_exec() {
    let text = bash_tool_description(&[shell_tool("shell_bash_exec", true)], 8, 1000);
    let bullet = text
        .lines()
        .find(|line| line.starts_with(CHANGE_REPORT))
        .expect("change report bullet");
    assert!(
        bullet.ends_with("(git status lists new files, which git diff leaves out)."),
        "{bullet}"
    );
    assert!(
        bullet.contains("base=$(shell_bash_exec --command 'git stash create')"),
        "{bullet}"
    );
    assert!(
        bullet
            .contains(r#"shell_bash_exec --command "git status --short; git diff ${base:-HEAD}""#),
        "{bullet}"
    );
    // It sits right after the shell-form bullet and before the tools bullet.
    let lines: Vec<_> = text.lines().collect();
    let at = lines.iter().position(|line| *line == bullet).unwrap();
    assert!(lines[at - 1].starts_with("- A tool with a shell form"));
    assert!(lines[at + 1].starts_with("- tools [quoted-glob]"));
}

#[test]
fn change_report_bullet_drops_the_snapshot_sentence_without_bash_exec() {
    let mut writer = shell_tool("fs_write", true);
    writer.schema["properties"] = json!({"path": {"type": "string"}, "diff": {"type": "string"}});
    let text = bash_tool_description(&[writer], 8, 1000);
    let bullet = text
        .lines()
        .find(|line| line.starts_with(CHANGE_REPORT))
        .expect("bullet");
    assert!(bullet.ends_with("(nothing to roll back to)."), "{bullet}");
    assert!(!text.contains("For one diff across several changes"));
}

#[test]
fn change_report_bullet_needs_a_shell_form_and_a_diff_property() {
    for tools in [
        vec![shell_tool("bash_exec", false)],
        vec![ToolSpec {
            output: None,
            ..shell_tool("bash_exec", true)
        }],
        vec![],
    ] {
        let text = bash_tool_description(&tools, 8, 1000);
        assert!(!text.contains("reports the change on stderr"), "{text}");
    }
}
