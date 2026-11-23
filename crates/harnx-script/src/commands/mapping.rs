//! Maps a tool's `structuredContent` to the stdout, stderr and exit status of
//! its script command, following the mapping the tool declared.

use serde_json::Value;

use crate::cli_map::OutputFormat;

/// Exit statuses a shell can report.
const EXIT_CODE_MIN: i64 = 0;
const EXIT_CODE_MAX: i64 = 255;

/// How one tool's structured result becomes command output.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct OutputMapping {
    /// structuredContent field printed on stdout.
    pub stdout: Option<String>,
    /// Format for an object item of the stdout field: `{name}` is replaced
    /// by that item's field.
    pub row: Option<String>,
    /// structuredContent field (string or array of strings) printed on stderr.
    pub stderr: Option<String>,
    /// structuredContent integer field used as the exit status.
    pub exit_code: Option<String>,
    /// Exit status for a tool error. Default 1.
    pub error_exit_code: Option<i32>,
    /// One line for `help <tool>`, e.g. "file text, like cat".
    pub summary: Option<String>,
}

pub(super) struct Mapped {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// None when `value` has no object `structuredContent`; the caller then
/// renders as before.
pub(super) fn apply(
    tool: &str,
    mapping: &OutputMapping,
    value: &Value,
    format: Option<OutputFormat>,
) -> Option<Mapped> {
    let structured = value.get("structuredContent").filter(|s| s.is_object())?;
    let stdout = match format {
        Some(OutputFormat::Json) => json_line(structured),
        _ => text_stdout(mapping, structured),
    };
    Some(Mapped {
        stdout,
        stderr: notes(tool, mapping, structured),
        exit_code: exit_code(mapping, structured),
    })
}

fn json_line(structured: &Value) -> String {
    format!("{structured}\n")
}

fn text_stdout(mapping: &OutputMapping, structured: &Value) -> String {
    let field = mapping
        .stdout
        .as_deref()
        .and_then(|name| structured.get(name));
    field
        .and_then(|field| field_text(field, mapping.row.as_deref()))
        .unwrap_or_else(|| json_line(structured))
}

/// None for a shape the mapping cannot print.
fn field_text(field: &Value, row: Option<&str>) -> Option<String> {
    match field {
        Value::String(text) => Some(terminated(text.clone())),
        Value::Array(items) => lines(items, row),
        _ => None,
    }
}

fn lines(items: &[Value], row: Option<&str>) -> Option<String> {
    let mut out = String::new();
    for item in items {
        let line = match (item, row) {
            (Value::String(text), _) => text.clone(),
            (Value::Object(_), Some(row)) => format_row(row, item),
            _ => return None,
        };
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
}

/// Replaces `{name}` with the item's field in one pass, so a value that
/// itself contains braces is never expanded again.
fn format_row(row: &str, item: &Value) -> String {
    let mut out = String::new();
    let mut rest = row;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                out.push_str(&scalar_text(item.get(&after[..end])));
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn scalar_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn notes(tool: &str, mapping: &OutputMapping, structured: &Value) -> String {
    let field = mapping
        .stderr
        .as_deref()
        .and_then(|name| structured.get(name));
    let notes: Vec<&str> = match field {
        Some(Value::String(note)) => vec![note.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    notes
        .into_iter()
        .filter(|note| !note.is_empty())
        .map(|note| format!("{tool}: {note}\n"))
        .collect()
}

impl OutputMapping {
    /// The exit status for a tool error, kept to what a shell can report
    /// and never 0, which would read as success.
    pub(crate) fn error_exit(&self) -> Option<i32> {
        self.error_exit_code
            .map(|code| clamp_exit(i64::from(code)).max(1))
    }
}

fn clamp_exit(code: i64) -> i32 {
    code.clamp(EXIT_CODE_MIN, EXIT_CODE_MAX) as i32
}

fn exit_code(mapping: &OutputMapping, structured: &Value) -> i32 {
    mapping
        .exit_code
        .as_deref()
        .and_then(|name| structured.get(name))
        .and_then(Value::as_i64)
        .map_or(0, clamp_exit)
}

fn terminated(mut text: String) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests;
