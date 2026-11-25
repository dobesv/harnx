//! The FIELDS section of `help <tool>`: the top-level properties of a tool's
//! declared result schema, for scripts that read `-o json`.

use serde_json::Value;

struct FieldRow {
    name: String,
    kind: String,
    description: String,
}

/// Append a FIELDS section when the schema has object `properties`. Nested
/// objects print as `object`; nothing recurses.
pub(super) fn fields_section(text: &mut String, schema: &Value) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let rows: Vec<FieldRow> = properties
        .iter()
        .map(|(name, property)| FieldRow {
            name: name.clone(),
            kind: type_text(property),
            description: first_line(property),
        })
        .collect();
    if rows.is_empty() {
        return;
    }
    let name_width = rows.iter().map(|row| row.name.chars().count()).max();
    let kind_width = rows.iter().map(|row| row.kind.chars().count()).max();
    text.push_str("\nFIELDS (-o json)\n");
    for row in &rows {
        let line = if row.description.is_empty() {
            format!("  {}  {}", pad(&row.name, name_width), row.kind)
        } else {
            format!(
                "  {}  {}  {}",
                pad(&row.name, name_width),
                pad(&row.kind, kind_width),
                row.description
            )
        };
        text.push_str(&line);
        text.push('\n');
    }
}

fn pad(text: &str, width: Option<usize>) -> String {
    let width = width.unwrap_or(0);
    format!("{text:<width$}")
}

fn type_text(property: &Value) -> String {
    match property.get("type") {
        Some(Value::String(kind)) if kind == "array" => {
            match property.get("items").and_then(|items| items.get("type")) {
                Some(Value::String(item)) => format!("array of {item}"),
                _ => "array".to_string(),
            }
        }
        Some(Value::String(kind)) => kind.clone(),
        Some(Value::Array(kinds)) => {
            let kinds: Vec<&str> = kinds
                .iter()
                .filter_map(Value::as_str)
                .filter(|kind| *kind != "null")
                .collect();
            if kinds.is_empty() {
                "any".to_string()
            } else {
                kinds.join(" or ")
            }
        }
        _ => "any".to_string(),
    }
}

fn first_line(property: &Value) -> String {
    property
        .get("description")
        .and_then(Value::as_str)
        .and_then(|description| {
            description
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .map(str::to_string)
        .unwrap_or_default()
}
