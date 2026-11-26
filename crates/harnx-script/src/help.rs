//! Deterministic schema-derived discovery text for registered tool commands.

use serde_json::Value;

use crate::commands::OutputMapping;
use crate::schema::{flag_table, properties, FlagKind, Property, Scalar};

/// One line of command syntax followed by the description's first sentence.
/// Required flags come first; spelling and types use the parser's lookup table.
pub fn synopsis(name: &str, schema: &Value, description: &str) -> String {
    let syntax = syntax(name, schema);
    let summary = first_sentence(description);
    if summary.is_empty() {
        syntax
    } else {
        format!("{syntax}  # {summary}")
    }
}

/// Man-page style help. Defaults are shown, not inserted by the argument parser.
/// Reference/union/nullable properties are labeled JSON, matching parse_args.
pub fn help(
    name: &str,
    schema: &Value,
    description: &str,
    output: Option<&OutputMapping>,
    output_schema: Option<&Value>,
) -> String {
    let properties = properties(schema);
    let mut text = header(name, schema, description);
    indent(&mut text, description.trim(), 4);
    text.push_str("\nOPTIONS\n");
    let conflict = flag_table(&properties).is_err();
    if conflict {
        text.push_str(
            "    Conflicting schema flag names; supply original property keys with --json.\n",
        );
    }
    for property in &properties {
        let option = if property_uses_json(property, conflict) {
            format!("--json <object> (property {:?})", property.key)
        } else {
            property.option()
        };
        text.push_str(&format!(
            "    {option}\n        type: {}; {}",
            property.kind.label(),
            if property.required {
                "required"
            } else {
                "optional"
            }
        ));
        if matches!(property.kind, FlagKind::Repeated(_)) {
            text.push_str("; repeat flag for each item");
        }
        metadata(&mut text, property.schema, property.value);
        if let Some(items) = property
            .value
            .get("items")
            .and_then(|items| items.get("enum"))
        {
            text.push_str(&format!("; item enum: {items}"));
        }
        text.push('\n');
        if let Some(description) = property.schema.get("description").and_then(Value::as_str) {
            indent(&mut text, description.trim(), 8);
        }
    }
    text.push_str("    --json <object> | --json -\n        Base object; '-' reads stdin. Flags override base keys in any order.\n");
    text.push_str("    -o json|text\n        Output rendering (handled by the command wrapper, not sent to the tool).\n");
    text.push_str("    --help\n        Show tool help (handled by the command wrapper).\n");
    text.push_str("    --\n        End options. Positional arguments aren't supported.\n");
    if let Some(mapping) = output {
        output_section(&mut text, mapping);
    }
    if let Some(schema) = output_schema {
        fields::fields_section(&mut text, schema);
    }
    text
}

/// NAME and SYNOPSIS, with the Python call line under the synopsis, and the
/// opening of DESCRIPTION.
fn header(name: &str, schema: &Value, description: &str) -> String {
    let mut text = format!(
        "NAME\n    {}\n\nSYNOPSIS\n    {}\n",
        synopsis_name(name, description),
        syntax(name, schema)
    );
    if let Some(line) = python::call_line(name, schema) {
        text.push_str(&format!("    PYTHON  {line}\n"));
    }
    text.push_str("\nDESCRIPTION\n");
    text
}

/// What the command prints where, for a tool with an output mapping.
fn output_section(text: &mut String, mapping: &OutputMapping) {
    text.push_str("\nOUTPUT\n");
    if let Some(summary) = &mapping.summary {
        indent(text, summary.trim(), 2);
    }
    let stderr = stderr_description(mapping);
    let field = |field: &Option<String>| field.as_ref().map(|f| format!("the `{f}` field"));
    let parts: Vec<String> = [
        ("stdout", field(&mapping.stdout)),
        ("stderr", stderr),
        ("exit status", field(&mapping.exit_code)),
    ]
    .into_iter()
    .filter_map(|(stream, text)| text.map(|text| format!("{stream}: {text}")))
    .collect();
    if !parts.is_empty() {
        text.push_str(&format!("  {}\n", parts.join("    ")));
    }
    text.push_str("  -o json prints every field.\n");
}

/// Where stderr comes from, in print order: the text, the notes, then the
/// after-text.
fn stderr_description(mapping: &OutputMapping) -> Option<String> {
    let notes_label = if mapping.stderr_text.is_some() {
        "notes from `"
    } else {
        "the `"
    };
    let notes_end = if mapping.stderr_text.is_some() {
        "`"
    } else {
        "` field"
    };
    let parts: Vec<String> = [
        (&mapping.stderr_text, "the `", "` field"),
        (&mapping.stderr, notes_label, notes_end),
        (&mapping.stderr_after, "the `", "` field"),
    ]
    .into_iter()
    .filter_map(|(name, before, after)| name.as_ref().map(|n| format!("{before}{n}{after}")))
    .collect();
    (!parts.is_empty()).then(|| parts.join(", then "))
}

fn property_uses_json(property: &Property<'_>, conflict: bool) -> bool {
    if conflict || property.flag.is_empty() {
        return true;
    }
    property.flag == "json"
}

fn has_required_json_property(properties: &[Property<'_>], conflict: bool) -> bool {
    properties
        .iter()
        .any(|p| p.required && property_uses_json(p, conflict))
}

fn collect_syntax_flags(properties: &mut [Property<'_>], parts: &mut Vec<String>) {
    properties.sort_by_key(|property| !property.required);
    for property in properties {
        if !property_uses_json(property, false) {
            parts.push(syntax_flag(property));
        }
    }
}

fn json_syntax_flag(required: bool) -> String {
    if required {
        "--json <object>".into()
    } else {
        "[--json <object>]".into()
    }
}

fn syntax(name: &str, schema: &Value) -> String {
    let mut properties = properties(schema);
    let conflict = flag_table(&properties).is_err();
    let json_required = has_required_json_property(&properties, conflict)
        || has_unlisted_required_key(schema, &properties);
    let mut parts = vec![name.split_whitespace().collect::<Vec<_>>().join(" ")];
    if !conflict {
        collect_syntax_flags(&mut properties, &mut parts);
    }
    parts.push(json_syntax_flag(json_required));
    parts.join(" ")
}

fn has_unlisted_required_key(schema: &Value, properties: &[Property<'_>]) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|key| !properties.iter().any(|property| property.key == key))
}

fn syntax_flag(property: &Property<'_>) -> String {
    let flag = match property.kind {
        FlagKind::Scalar(Scalar::Boolean) => format!("--{}|--no-{}", property.flag, property.flag),
        FlagKind::Scalar(kind) | FlagKind::Repeated(kind) => {
            format!("--{} <{}>", property.flag, kind.label())
        }
        FlagKind::Json => format!("--{} <json>", property.flag),
    };
    let repeated = matches!(property.kind, FlagKind::Repeated(_));
    if property.required {
        format!("{flag}{}", if repeated { "..." } else { "" })
    } else {
        format!("[{flag}]{}", if repeated { "..." } else { "" })
    }
}

fn synopsis_name(name: &str, description: &str) -> String {
    let summary = first_sentence(description);
    if summary.is_empty() {
        name.into()
    } else {
        format!("{name} - {summary}")
    }
}

/// `default` lives on the declared schema; `enum` usually sits on the
/// non-null branch of a nullable union, so it is read from there first.
fn metadata(text: &mut String, declared: &Value, value: &Value) {
    if let Some(default) = declared.get("default") {
        text.push_str(&format!("; default: {default}"));
    }
    if let Some(choices) = value.get("enum").or_else(|| declared.get("enum")) {
        text.push_str(&format!("; enum: {choices}"));
    }
}

fn indent(text: &mut String, description: &str, spaces: usize) {
    for line in description.lines() {
        text.push_str(&" ".repeat(spaces));
        text.push_str(line);
        text.push('\n');
    }
}

fn first_sentence(description: &str) -> String {
    let line = description.split_whitespace().collect::<Vec<_>>().join(" ");
    for (index, ch) in line.char_indices() {
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }
        let rest = &line[index + ch.len_utf8()..];
        let terminal = rest.is_empty() || rest.starts_with(char::is_whitespace);
        if terminal {
            return line[..index + ch.len_utf8()].into();
        }
    }
    line
}

mod fields;
mod python;
#[cfg(test)]
mod tests;
