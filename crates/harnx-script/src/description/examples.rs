use serde_json::Value;

use crate::{
    commands::ToolSpec,
    schema::{flag_table, is_complex, properties, scalar, Scalar},
};

pub(super) fn pipeline(tools: &[&ToolSpec]) -> String {
    let reader = tools.iter().find(|tool| {
        ["fs_read", "read_file", "file_read"]
            .iter()
            .any(|suffix| tool.name.ends_with(suffix))
            && string_option(tool, "path", Some("src/lib.rs")).is_some()
            && required_only(tool, &["path"])
    });
    let (name, instruction) = match reader {
        Some(tool) => (
            shell_word(&tool.name),
            "Use a path this tool can read; filter matching lines and keep three:",
        ),
        None => (
            "fs_read".into(),
            "Generic illustration, not an extra installed tool: requires an fs_read --path command in bash_tools:",
        ),
    };
    format!("\nWorked pipeline: {instruction}\n```bash\n{name} --path src/lib.rs -o text | rg -n 'TODO' | head -3\n```\n")
}

/// Keep the idiom accurate for older text-only attachment toolsets and custom
/// creators whose schemas don't accept the PNG/base64 flags.
pub(super) fn attachment_idiom(tools: &[&ToolSpec]) -> Option<String> {
    let tool = tools.iter().find(|tool| {
        tool.name.ends_with("attachment_create")
            && string_option(tool, "encoding", Some("base64")).is_some()
            && string_option(tool, "mime-type", Some("image/png")).is_some()
            && string_option(tool, "content", None).is_some()
            && required_only(tool, &["encoding", "mime-type", "content"])
    })?;
    Some(format!(
        "{} --mime-type image/png --encoding base64 --content \"$(base64 -w0 /tmp/x.png)\"",
        shell_word(&tool.name)
    ))
}

fn string_option<'a>(tool: &'a ToolSpec, flag: &str, value: Option<&str>) -> Option<&'a Value> {
    if is_complex(&tool.schema) {
        return None;
    }
    let props = properties(&tool.schema);
    flag_table(&props).ok()?;
    let property = props.iter().find(|prop| prop.flag == flag)?;
    let schema = property.schema;
    if scalar(schema) != Some(Scalar::String) {
        return None;
    }
    if let (Some(value), Some(choices)) = (value, schema.get("enum").and_then(Value::as_array)) {
        if !choices.iter().any(|choice| choice.as_str() == Some(value)) {
            return None;
        }
    }
    Some(schema)
}

fn required_only(tool: &ToolSpec, flags: &[&str]) -> bool {
    let props = properties(&tool.schema);
    tool.schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .all(|key| {
            props
                .iter()
                .any(|prop| key.as_str() == Some(prop.key) && flags.contains(&prop.flag.as_str()))
        })
}

fn shell_word(word: &str) -> String {
    if word
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':'))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}
