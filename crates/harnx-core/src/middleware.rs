//! Strict middleware declarations and the resolved chain shared by runtime consumers.

mod fetch;
use fetch::parse_fetch_hosts;

use std::{collections::HashSet, path::PathBuf, time::Duration};

use fancy_regex::Regex;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

/// A file's ordered declarations. Origin is attached by the file/package loader.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct MiddlewareConfig {
    pub entries: Vec<MiddlewareEntry>,
    #[serde(skip)]
    pub origin: Option<MiddlewareOrigin>,
}

impl<'de> Deserialize<'de> for MiddlewareConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let entries = Vec::<MiddlewareEntry>::deserialize(deserializer)?;
        let mut names = HashSet::new();
        for entry in &entries {
            if !names.insert(entry.name()) {
                return Err(D::Error::custom(format!(
                    "middleware '{}': duplicate name in the same configuration level",
                    entry.name()
                )));
            }
        }
        Ok(Self {
            entries,
            origin: None,
        })
    }
}

impl MiddlewareConfig {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum MiddlewareEntry {
    Full(MiddlewareDefinition),
    Disable { name: String, enabled: bool },
}

impl MiddlewareEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Full(definition) => &definition.name,
            Self::Disable { name, .. } => name,
        }
    }
}

/// Full replacement, never a partial overlay of an earlier declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MiddlewareDefinition {
    pub name: String,
    #[serde(rename = "match")]
    pub matcher: String,
    #[serde(flatten)]
    pub source: MiddlewareSourceConfig,
    pub tools: Vec<String>,
    /// Exact HTTP(S) hosts; empty denies all fetch requests.
    pub fetch: Vec<String>,
    #[serde(serialize_with = "serialize_timeout")]
    pub timeout: Duration,
    pub on_error: MiddlewareOnError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum MiddlewareSourceConfig {
    Bash { bash: String },
    Python { python: String },
    Script { script: PathBuf },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MiddlewareOnError {
    #[default]
    Deny,
    Allow,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMiddlewareEntry {
    name: String,
    #[serde(rename = "match")]
    matcher: Option<String>,
    bash: Option<String>,
    python: Option<String>,
    script: Option<PathBuf>,
    tools: Option<Vec<String>>,
    fetch: Option<Vec<String>>,
    timeout: Option<String>,
    on_error: Option<MiddlewareOnError>,
    enabled: Option<bool>,
}

impl<'de> Deserialize<'de> for MiddlewareEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Decode through a value so every field error (including unknown keys)
        // identifies the entry, not just its index in a list.
        let value = serde_json::Value::deserialize(deserializer)?;
        let name = value
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        parse_entry(&value)
            .map_err(|error| D::Error::custom(format!("middleware '{name}': {error}")))
    }
}

const DEFAULT_MIDDLEWARE_TIMEOUT_SECS: u64 = 30;

fn parse_entry(value: &serde_json::Value) -> Result<MiddlewareEntry, String> {
    let raw: RawMiddlewareEntry =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if let Some(fields) = value.as_object() {
        if let Some((field, _)) = fields.iter().find(|(_, value)| value.is_null()) {
            return Err(format!("{field} must not be null"));
        }
    }
    if raw.name.trim().is_empty() {
        return Err("name must not be empty".into());
    }
    if raw.enabled == Some(false) {
        if value.as_object().is_some_and(|fields| fields.len() != 2) {
            return Err("a disable entry accepts only name and enabled: false".into());
        }
        return Ok(MiddlewareEntry::Disable {
            name: raw.name,
            enabled: false,
        });
    }
    let matcher = raw.matcher.ok_or("a full entry requires match")?;
    Regex::new(&matcher).map_err(|e| format!("invalid match regex: {e}"))?;
    let source = match (raw.bash, raw.python, raw.script) {
        (Some(bash), None, None) => MiddlewareSourceConfig::Bash { bash },
        (None, Some(python), None) => MiddlewareSourceConfig::Python { python },
        (None, None, Some(script)) => {
            script_language(&script)?;
            MiddlewareSourceConfig::Script { script }
        }
        _ => return Err("a full entry requires exactly one of bash, python or script".into()),
    };
    let timeout = raw
        .timeout
        .map(|text| humantime::parse_duration(&text).map_err(|e| format!("invalid timeout: {e}")))
        .transpose()?
        .unwrap_or(Duration::from_secs(DEFAULT_MIDDLEWARE_TIMEOUT_SECS));
    if timeout.is_zero() {
        return Err("timeout must be greater than zero".into());
    }
    Ok(MiddlewareEntry::Full(MiddlewareDefinition {
        name: raw.name,
        matcher,
        source,
        tools: raw.tools.unwrap_or_default(),
        fetch: parse_fetch_hosts(raw.fetch.unwrap_or_default())?,
        timeout,
        on_error: raw.on_error.unwrap_or_default(),
    }))
}

fn serialize_timeout<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&humantime::format_duration(*duration).to_string())
}

/// Whether a script file is Python; rejects extensions with no defined interpreter.
pub fn script_language(path: &std::path::Path) -> Result<bool, String> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("sh" | "bash") => Ok(false),
        Some("py") => Ok(true),
        _ => Err(format!(
            "script '{}' must have a .sh, .bash or .py extension",
            path.display()
        )),
    }
}

/// Declaring file and the directory against which script paths are resolved.
/// Package declarations use the package root, not its agents/tool_servers subdirectory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiddlewareOrigin {
    pub file: PathBuf,
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum MiddlewareSource {
    Bash(String),
    Python(String),
}

#[derive(Debug, Clone)]
pub struct ResolvedMiddleware {
    pub name: String,
    pub matcher: Regex,
    pub source: MiddlewareSource,
    pub tools: Vec<String>,
    /// Exact HTTP(S) hosts; empty denies all fetch requests.
    pub fetch: Vec<String>,
    /// Inclusive wall-clock limit, including time spent in downstream calls.
    pub timeout: Duration,
    pub on_error: MiddlewareOnError,
    pub origin: MiddlewareOrigin,
}

#[derive(Debug, Clone)]
pub struct ResolvedMiddlewareChain {
    pub entries: Vec<ResolvedMiddleware>,
    /// SHA-256 of ordered effective settings and source text, excluding file locations.
    pub version: String,
}

impl ResolvedMiddlewareChain {
    // qual:allow(complexity, error_handling) reason: "concrete settings contain only infallible JSON strings, lists, enums and integers"
    pub fn new(entries: Vec<ResolvedMiddleware>) -> Self {
        // Tuples give a fixed field order regardless of serde_json/preserve_order.
        // Include the source language as well as its text: identical Bash and
        // Python text does not describe the same program.
        let settings: Vec<_> = entries
            .iter()
            .map(|entry| {
                (
                    &entry.name,
                    entry.matcher.as_str(),
                    &entry.source,
                    &entry.tools,
                    entry.timeout.as_secs(),
                    entry.timeout.subsec_nanos(),
                    entry.on_error,
                )
            })
            .collect();
        let mut bytes = serde_json::to_vec(&settings).expect("middleware settings serialize");
        // Preserve pre-fetch chain versions when no entry grants network access.
        if entries.iter().any(|entry| !entry.fetch.is_empty()) {
            bytes.extend_from_slice(b"\0fetch\0");
            bytes.extend(fetch_settings_bytes(&entries));
        }
        let version = crate::crypto::hex_encode(&Sha256::digest(bytes));
        Self { entries, version }
    }
}

fn fetch_settings_bytes(entries: &[ResolvedMiddleware]) -> Vec<u8> {
    let hosts: Vec<_> = entries.iter().map(|entry| &entry.fetch).collect();
    serde_json::to_vec(&hosts).expect("fetch settings serialize")
}

#[cfg(test)]
mod tests;
