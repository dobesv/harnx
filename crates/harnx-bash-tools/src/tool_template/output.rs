use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use super::type_mismatch;

pub(crate) const HEAD_LINES_DESCRIPTION: &str =
    "Return only the first N lines of each stream. Prefer this over `| head -N` in the command.";
pub(crate) const TAIL_LINES_DESCRIPTION: &str =
    "Return only the last N lines of each stream. Prefer this over `| tail -N` in the command.";
pub(crate) const MAX_OUTPUT_BYTES_DESCRIPTION: &str = "Truncate output to this many bytes. Per-line clipping is off by default; long lines are bounded by the byte cap and line window. Prefer this over `| head -c N` in the command.";

pub(crate) const OUTPUT_CONTROL_PARAMS: [(&str, &str); 3] = [
    ("head_lines", HEAD_LINES_DESCRIPTION),
    ("tail_lines", TAIL_LINES_DESCRIPTION),
    ("max_output_bytes", MAX_OUTPUT_BYTES_DESCRIPTION),
];

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemplateOutputOptions {
    #[serde(default, deserialize_with = "deserialize_output_default")]
    pub(crate) head_lines: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_output_default")]
    pub(crate) tail_lines: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_output_default")]
    pub(crate) max_output_bytes: Option<usize>,
}

fn deserialize_output_default<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: Deserializer<'de>,
{
    // Missing defaults are optional, but authored null values aren't integers.
    usize::deserialize(deserializer).map(Some)
}

#[derive(Debug)]
pub(crate) struct BoundTemplateArgs {
    pub(crate) env: HashMap<String, String>,
    pub(crate) output: TemplateOutputOptions,
}

impl TemplateOutputOptions {
    pub(crate) fn with_defaults(self, defaults: &Self) -> Self {
        Self {
            head_lines: self.head_lines.or(defaults.head_lines),
            tail_lines: self.tail_lines.or(defaults.tail_lines),
            max_output_bytes: self.max_output_bytes.or(defaults.max_output_bytes),
        }
    }

    pub(crate) fn from_args(args: &Map<String, Value>) -> Result<Self> {
        Ok(Self {
            head_lines: parse_output_control(args, "head_lines")?,
            tail_lines: parse_output_control(args, "tail_lines")?,
            max_output_bytes: parse_output_control(args, "max_output_bytes")?,
        })
    }
}

pub(crate) fn is_output_control(name: &str) -> bool {
    OUTPUT_CONTROL_PARAMS.iter().any(|(key, _)| *key == name)
}

pub(crate) fn add_output_controls(
    properties: &mut Map<String, Value>,
    defaults: &TemplateOutputOptions,
) -> Result<()> {
    let defaults = [
        defaults.head_lines,
        defaults.tail_lines,
        defaults.max_output_bytes,
    ];
    for ((name, description), default) in OUTPUT_CONTROL_PARAMS.into_iter().zip(defaults) {
        if properties.contains_key(name) {
            anyhow::bail!("parameter `{name}` is reserved for output control");
        }
        let mut schema =
            serde_json::json!({"type": "integer", "minimum": 0, "description": description});
        if let Some(default) = default {
            schema["default"] = default.into();
        }
        properties.insert(name.to_string(), schema);
    }
    Ok(())
}
fn parse_output_control(args: &Map<String, Value>, name: &str) -> Result<Option<usize>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .map(Some)
            .ok_or_else(|| type_mismatch(name, "a non-negative integer", value)),
    }
}

#[cfg(test)]
mod tests;
