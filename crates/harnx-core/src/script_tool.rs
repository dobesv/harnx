//! User-defined tools backed by in-process BashKit or Monty Python scripts.

use crate::json_schema::JsonSchema;
use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Portable tool-name limit, including providers with a 64-character maximum.
pub const MAX_SCRIPT_TOOL_NAME_LEN: usize = 64;

const DEFAULT_SCRIPT_TOOL_TIMEOUT_SECS: u64 = 300;

/// Serialized definition shared by global config and one-definition package files.
/// Schema compilation and cross-source checks belong to the runtime loader.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScriptToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: JsonSchema,
    /// Literal names, globs or toolsets; empty permits no inner calls.
    pub tools: Vec<String>,
    #[serde(flatten)]
    pub source: ScriptToolSource,
    #[serde(serialize_with = "serialize_timeout")]
    pub timeout: Duration,
    pub output: ScriptToolOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ScriptToolSource {
    Bash { bash: String },
    Python { python: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptToolOutput {
    #[default]
    Text,
    Json,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScriptToolDefinition {
    name: String,
    description: String,
    parameters: JsonSchema,
    #[serde(default)]
    tools: Vec<String>,
    bash: Option<String>,
    python: Option<String>,
    timeout: Option<String>,
    #[serde(default)]
    output: ScriptToolOutput,
}

impl<'de> Deserialize<'de> for ScriptToolDefinition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Keep names in field errors, including errors from deny_unknown_fields.
        let value = serde_json::Value::deserialize(deserializer)?;
        let name = value
            .get("name")
            .and_then(|name| name.as_str())
            .unwrap_or("<unnamed>");
        parse_definition(&value)
            .map_err(|error| D::Error::custom(format!("script tool '{name}': {error}")))
    }
}

fn parse_definition(value: &serde_json::Value) -> Result<ScriptToolDefinition, String> {
    let raw: RawScriptToolDefinition =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    crate::middleware::validate_entry_shape(value, &raw.name)?;
    validate_script_tool_name(&raw.name)?;
    let source = match (raw.bash, raw.python) {
        (Some(bash), None) => ScriptToolSource::Bash { bash },
        (None, Some(python)) => ScriptToolSource::Python { python },
        _ => return Err("requires exactly one of bash or python".into()),
    };
    let timeout =
        crate::middleware::parse_entry_timeout(raw.timeout, DEFAULT_SCRIPT_TOOL_TIMEOUT_SECS)?;
    Ok(ScriptToolDefinition {
        name: raw.name,
        description: raw.description,
        parameters: raw.parameters,
        tools: raw.tools,
        source,
        timeout,
        output: raw.output,
    })
}

/// Validate without sanitizing: changing a declared name could hide a collision.
pub fn validate_script_tool_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > MAX_SCRIPT_TOOL_NAME_LEN
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(format!(
            "name must contain 1-{MAX_SCRIPT_TOOL_NAME_LEN} characters from [A-Za-z0-9_-]"
        ));
    }
    Ok(())
}

fn serialize_timeout<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&humantime::format_duration(*duration).to_string())
}

#[cfg(test)]
mod tests;
