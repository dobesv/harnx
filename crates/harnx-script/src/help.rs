//! Deterministic schema-derived discovery text for registered tool commands.

use serde_json::Value;

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
pub fn help(name: &str, schema: &Value, description: &str) -> String {
    let properties = properties(schema);
    let mut text = format!(
        "NAME\n    {}\n\nSYNOPSIS\n    {}\n\nDESCRIPTION\n",
        synopsis_name(name, description),
        syntax(name, schema)
    );
    indent(&mut text, description.trim(), 4);
    text.push_str("\nOPTIONS\n");
    let conflict = flag_table(&properties).is_err();
    if conflict {
        text.push_str(
            "    Conflicting schema flag names; supply original property keys with --json.\n",
        );
    }
    for property in &properties {
        let option = if conflict || property.flag == "json" || property.flag.is_empty() {
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
        metadata(&mut text, property.schema);
        if let Some(items) = property
            .schema
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
    text
}

fn syntax(name: &str, schema: &Value) -> String {
    let mut properties = properties(schema);
    let conflict = flag_table(&properties).is_err();
    let json_required = properties.iter().any(|property| {
        property.required && (conflict || property.flag == "json" || property.flag.is_empty())
    });
    let mut parts = vec![name.split_whitespace().collect::<Vec<_>>().join(" ")];
    if !conflict {
        properties.sort_by_key(|property| !property.required);
        for property in &properties {
            if property.flag != "json" && !property.flag.is_empty() {
                parts.push(syntax_flag(property));
            }
        }
    }
    parts.push(
        if json_required {
            "--json <object>"
        } else {
            "[--json <object>]"
        }
        .into(),
    );
    parts.join(" ")
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

fn metadata(text: &mut String, schema: &Value) {
    for key in ["default", "enum"] {
        if let Some(value) = schema.get(key) {
            text.push_str(&format!("; {key}: {value}"));
        }
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
        let rest = &line[index + ch.len_utf8()..];
        if matches!(ch, '.' | '!' | '?')
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return line[..index + ch.len_utf8()].into();
        }
    }
    line
}

#[cfg(test)]
mod tests;
