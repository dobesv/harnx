//! YAML-defined shell command templates (GitHub #48). Not to be confused with
//! `tool_templates.rs`, which holds MCP call-display strings.

// Wired up in task 6a367e89 and the other shell-command-template follow-up tasks.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use minijinja::{Environment, UndefinedBehavior};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::server::{BUILTIN_TOOL_NAMES, COMMAND_TIMEOUT_ARG, DEFAULT_COMMAND_TIMEOUT_SECS};

#[derive(Clone, Debug, Deserialize)]
pub struct ToolTemplate {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) parameters: BTreeMap<String, ParameterDef>,
    pub(crate) env: Option<HashMap<String, String>>,
    pub(crate) sandbox: Option<SandboxConfig>,
    #[serde(default)]
    pub(crate) template: bool,
    pub(crate) script: String,
}

impl ToolTemplate {
    pub(crate) fn input_schema(&self) -> Result<serde_json::Value> {
        let mut properties = Map::new();
        let mut required = Vec::new();

        for (name, parameter) in &self.parameters {
            if parameter.required {
                required.push(Value::String(name.clone()));
            }
            properties.insert(
                name.clone(),
                Value::Object(schema_for_parameter(name, parameter)?),
            );
        }
        properties.insert(
            COMMAND_TIMEOUT_ARG.to_string(),
            json!({
                "type": "integer",
                "minimum": 0,
                "description": format!(
                    "Kill the command after this many seconds. Defaults to {DEFAULT_COMMAND_TIMEOUT_SECS} (24 hours); use 0 for no deadline."
                )
            }),
        );

        Ok(json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }))
    }

    pub(crate) fn compiled_patterns(&self) -> anyhow::Result<BTreeMap<String, regex::Regex>> {
        self.parameters
            .iter()
            .filter_map(|(name, parameter)| {
                parameter.pattern.as_ref().map(|pattern| {
                    regex::Regex::new(pattern)
                        .with_context(|| format!("invalid pattern for parameter `{name}`"))
                        .map(|compiled| (name.clone(), compiled))
                })
            })
            .collect()
    }

    pub(crate) fn validate_and_bind(
        &self,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<HashMap<String, String>> {
        for name in args.keys() {
            if !self.parameters.contains_key(name) {
                anyhow::bail!("unknown argument `{name}`: no parameter with that name");
            }
        }

        let compiled_patterns = self.compiled_patterns()?;
        let mut bindings = HashMap::new();

        for (name, parameter) in &self.parameters {
            let value = match args.get(name) {
                Some(value) => value.clone(),
                None if parameter.required => {
                    anyhow::bail!("missing required parameter `{name}`");
                }
                None => match &parameter.default {
                    Some(default) => yaml_value_to_json(default)
                        .with_context(|| format!("invalid default for parameter `{name}`"))?,
                    None => continue,
                },
            };

            let env_value = parameter.ty.env_value(name, &value)?;

            if let Some(allowed) = &parameter.r#enum {
                let allowed = allowed
                    .iter()
                    .map(yaml_value_to_json)
                    .collect::<Result<Vec<_>>>()
                    .with_context(|| format!("invalid enum for parameter `{name}`"))?;
                if !allowed.contains(&value) {
                    anyhow::bail!(
                        "invalid value for parameter `{name}`: {value} is not one of the allowed enum values"
                    );
                }
            }

            if let Some(pattern) = compiled_patterns.get(name) {
                if !pattern.is_match(&env_value) {
                    anyhow::bail!(
                        "invalid value for parameter `{name}`: `{env_value}` does not match pattern `{}`",
                        pattern.as_str()
                    );
                }
            }

            parameter.validate_range(name, &value)?;
            bindings.insert(param_to_env_name(name)?, env_value);
        }

        Ok(bindings)
    }

    pub(crate) fn render_script(&self, bound: &HashMap<String, String>) -> Result<String> {
        if !self.template {
            return Ok(self.script.clone());
        }

        let mut context = HashMap::new();
        for name in self.parameters.keys() {
            let env_name = param_to_env_name(name)?;
            if let Some(value) = bound.get(&env_name) {
                context.insert(name, value);
            }
        }

        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Lenient);
        env.add_filter("quote", |value: &str| -> String {
            format!("'{}'", value.replace('\'', r#"'\''"#))
        });
        env.render_str(&self.script, context).map_err(Into::into)
    }
}

fn schema_for_parameter(name: &str, parameter: &ParameterDef) -> Result<Map<String, Value>> {
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!(parameter.ty.schema_type()));

    if let Some(description) = &parameter.description {
        schema.insert("description".to_string(), json!(description));
    }
    if let Some(values) = &parameter.r#enum {
        let values = values
            .iter()
            .map(yaml_value_to_json)
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("invalid enum for parameter `{name}`"))?;
        schema.insert("enum".to_string(), Value::Array(values));
    }

    match parameter.ty {
        ParamType::String => add_string_constraints(&mut schema, parameter),
        ParamType::Integer | ParamType::Number => add_numeric_constraints(&mut schema, parameter),
        ParamType::Boolean => {}
    }

    Ok(schema)
}

fn add_string_constraints(schema: &mut Map<String, Value>, parameter: &ParameterDef) {
    if let Some(min) = parameter.min {
        schema.insert("minLength".to_string(), json!(min.ceil().max(0.0) as u64));
    }
    if let Some(max) = parameter.max {
        schema.insert("maxLength".to_string(), json!(max.floor().max(0.0) as u64));
    }
    if let Some(pattern) = &parameter.pattern {
        // JSON Schema pattern uses regex-crate syntax here: ECMA-262-ish,
        // without PCRE features such as lookbehind.
        schema.insert("pattern".to_string(), json!(pattern));
    }
}

fn add_numeric_constraints(schema: &mut Map<String, Value>, parameter: &ParameterDef) {
    if let Some(min) = parameter.min {
        schema.insert("minimum".to_string(), json!(min));
    }
    if let Some(max) = parameter.max {
        schema.insert("maximum".to_string(), json!(max));
    }
}

fn yaml_value_to_json(value: &serde_yaml::Value) -> Result<Value> {
    serde_json::to_value(value).context("YAML value is not JSON-compatible")
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ParameterDef {
    #[serde(rename = "type")]
    pub(crate) ty: ParamType,
    #[serde(default)]
    pub(crate) required: bool,
    pub(crate) description: Option<String>,
    pub(crate) pattern: Option<String>,
    #[serde(rename = "enum")]
    pub(crate) r#enum: Option<Vec<serde_yaml::Value>>,
    pub(crate) min: Option<f64>,
    pub(crate) max: Option<f64>,
    pub(crate) default: Option<serde_yaml::Value>,
}

impl ParameterDef {
    fn validate_range(&self, name: &str, value: &Value) -> Result<()> {
        let (measured, measurement) = match self.ty {
            ParamType::Integer | ParamType::Number => (
                value
                    .as_f64()
                    .expect("numeric parameter should have been type-checked"),
                "value",
            ),
            ParamType::String => (
                value
                    .as_str()
                    .expect("string parameter should have been type-checked")
                    .chars()
                    .count() as f64,
                "character length",
            ),
            ParamType::Boolean => return Ok(()),
        };

        if let Some(min) = self.min {
            if measured < min {
                anyhow::bail!(
                    "invalid value for parameter `{name}`: {measurement} must be at least {min}"
                );
            }
        }
        if let Some(max) = self.max {
            if measured > max {
                anyhow::bail!(
                    "invalid value for parameter `{name}`: {measurement} must be at most {max}"
                );
            }
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
}

impl ParamType {
    fn schema_type(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }

    fn env_value(&self, name: &str, value: &Value) -> Result<String> {
        let converted = match self {
            Self::Integer => match value {
                Value::Number(number) if number.is_i64() || number.is_u64() => number.to_string(),
                _ => return Err(type_mismatch(name, self.schema_type(), value)),
            },
            Self::Number => match value {
                Value::Number(number) => number.to_string(),
                _ => return Err(type_mismatch(name, self.schema_type(), value)),
            },
            Self::Boolean => match value {
                Value::Bool(boolean) => boolean.to_string(),
                _ => return Err(type_mismatch(name, self.schema_type(), value)),
            },
            Self::String => match value {
                Value::String(string) => string.clone(),
                _ => return Err(type_mismatch(name, self.schema_type(), value)),
            },
        };

        Ok(converted)
    }
}

fn type_mismatch(name: &str, expected: &str, value: &Value) -> anyhow::Error {
    anyhow::anyhow!(
        "invalid type for parameter `{name}`: expected {expected}, got {}",
        json_type_name(value)
    )
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SandboxConfig {
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default = "default_true")]
    pub(crate) network: bool,
    #[serde(default)]
    pub(crate) read: Vec<String>,
    #[serde(default)]
    pub(crate) write: Vec<String>,
    #[serde(default)]
    pub(crate) env: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            network: true,
            read: Vec::new(),
            write: Vec::new(),
            env: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// Transform a template parameter name into an `UPPER_SNAKE_CASE` environment variable name.
///
/// # Security boundary
///
/// This function enforces the core injection-proofing guarantee for `template: false` mode:
/// agent-supplied values reach the script *only* as env vars, never spliced into shell text.
/// That guarantee assumes harnx controls env-var *names*. If an agent could set `PATH`,
/// `LD_PRELOAD`, or `PS4` via a parameter, the agent could alter process behavior or
/// execute arbitrary code—even with a fixed script body.
///
/// The critical-variable list blocks:
/// - `BASH_FUNC*` prefix: shellshock function import
/// - `PATH`, `IFS`, `HOME`, `PWD`: path/glob resolution
/// - `LD_PRELOAD`, `LD_LIBRARY_PATH`: dynamic linker injection
/// - `BASH_ENV`, `ENV`: startup file execution
/// - `SHELLOPTS`: shell option changes
/// - `PS4`: bash evaluates `$(...)` inside `PS4` under `set -x` (xtrace) even for
///   fixed scripts, making `PS4='$(malicious-command)'` equivalent to arbitrary
///   code execution
/// - `BASH_XTRACEFD`: redirect trace output
/// - `CDPATH`, `GLOBIGNORE`: path/glob behavior
/// - `BASHOPTS`: shell options
/// - `TIMEOUT_SECS`: harnx-owned command execution metadata
///
/// Anyone extending or relaxing this list must treat it as a security boundary,
/// not an ergonomic convenience. A new entry should only be added after confirming
/// the variable cannot evaluate code or escape sandbox confinement when set by
/// an untrusted agent.
///
/// See also: README "Threat model and security framing" and the reserved env names list.
pub(crate) fn param_to_env_name(param: &str) -> Result<String> {
    if !param.is_ascii() {
        anyhow::bail!("invalid parameter name `{param}`: must contain ASCII characters only");
    }

    let env_name = param.to_uppercase().replace('-', "_");

    if env_name.starts_with("BASH_FUNC") {
        anyhow::bail!(
            "invalid parameter name `{param}`: transformed environment variable name `{env_name}` uses reserved BASH_FUNC prefix"
        );
    }

    const CRITICAL_ENV_VARS: &[&str] = &[
        "PATH",
        "IFS",
        "HOME",
        "PWD",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "BASH_ENV",
        "ENV",
        "SHELLOPTS",
        "PS4",
        "BASH_XTRACEFD",
        "CDPATH",
        "GLOBIGNORE",
        "BASHOPTS",
        "TIMEOUT_SECS",
    ];
    if CRITICAL_ENV_VARS.contains(&env_name.as_str()) {
        anyhow::bail!(
            "invalid parameter name `{param}`: transformed environment variable name `{env_name}` collides with a critical environment variable"
        );
    }

    let Some(first) = env_name.chars().next() else {
        anyhow::bail!(
            "invalid parameter name `{param}`: transformed environment variable name is empty"
        );
    };
    if !first.is_ascii_uppercase() && first != '_' {
        anyhow::bail!(
            "invalid parameter name `{param}`: transformed environment variable name `{env_name}` must start with A-Z or underscore"
        );
    }
    if !env_name.chars().skip(1).all(|character| {
        character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
    }) {
        anyhow::bail!(
            "invalid parameter name `{param}`: transformed environment variable name `{env_name}` may contain only A-Z, 0-9, or underscore"
        );
    }

    Ok(env_name)
}

fn validate_env_name_uniqueness(template: &ToolTemplate) -> Result<()> {
    // Validate that all parameter→env-name mappings within a template are unique and
    // that no parameter's transformed env name collides with a top-level `env:` key.
    //
    // Without this check, two params (e.g., `file-path` and `file_path`) could both
    // transform to `FILE_PATH`, causing `validate_and_bind` to silently overwrite one
    // binding with the other. Similarly, a param named `foo` would collide with a
    // top-level `env: { FOO: ... }` fixed injection.
    //
    // Both cases would let an agent bypass declared validation constraints or
    // overwrite author-fixed env values—undermining the security boundary.
    let mut parameters_by_env_name = HashMap::new();

    for parameter in template.parameters.keys() {
        let env_name = param_to_env_name(parameter)?;
        if let Some(first_parameter) =
            parameters_by_env_name.insert(env_name.clone(), parameter.as_str())
        {
            anyhow::bail!(
                "parameters `{first_parameter}` and `{parameter}` both map to environment variable `{env_name}`"
            );
        }
        if template
            .env
            .as_ref()
            .is_some_and(|env| env.contains_key(&env_name))
        {
            anyhow::bail!(
                "parameter `{parameter}` maps to environment variable `{env_name}`, which clashes with top-level `env:` entry `{env_name}`"
            );
        }
    }

    Ok(())
}

#[derive(Debug)]
struct DiscoveredTemplate {
    template: ToolTemplate,
    path: PathBuf,
    explicit: bool,
}

pub(crate) fn discover_templates(
    package_dir: Option<&Path>,
    cli_files: &[PathBuf],
    cli_dirs: &[PathBuf],
) -> Result<Vec<ToolTemplate>> {
    let mut discovered = Vec::new();
    let mut indexes = HashMap::new();

    if let Some(package_dir) = package_dir {
        let auto_dir = package_dir.join("bash_tools");
        let paths = sorted_yaml_files(&auto_dir, true)?;
        let auto = load_template_pass(&paths, false, true)?;
        merge_template_pass(&mut discovered, &mut indexes, auto)?;
    }

    let explicit_files = load_template_pass(cli_files, true, false)?;
    merge_template_pass(&mut discovered, &mut indexes, explicit_files)?;

    for cli_dir in cli_dirs {
        let paths = sorted_yaml_files(cli_dir, false)?;
        // A named directory is explicit input. Fail on malformed entries so a typo
        // cannot silently remove one of the tools the user requested.
        let explicit_dir = load_template_pass(&paths, true, false)?;
        merge_template_pass(&mut discovered, &mut indexes, explicit_dir)?;
    }

    Ok(discovered
        .into_iter()
        .map(|discovered| discovered.template)
        .collect())
}

fn sorted_yaml_files(dir: &Path, missing_is_empty: bool) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if missing_is_empty && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", dir.display()));
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("yaml") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn load_template_pass(
    paths: &[PathBuf],
    explicit: bool,
    skip_invalid: bool,
) -> Result<Vec<DiscoveredTemplate>> {
    let mut loaded = Vec::new();
    let mut names = HashMap::<String, PathBuf>::new();

    for path in paths {
        let template = match load_template_file(path) {
            Ok(template) => template,
            Err(error) if skip_invalid => {
                log::warn!(
                    "skipping invalid auto-discovered tool template {}: {error:#}",
                    path.display()
                );
                continue;
            }
            Err(error) => return Err(error),
        };

        if BUILTIN_TOOL_NAMES.contains(&template.name.as_str()) {
            if explicit {
                anyhow::bail!(
                    "tool template {} uses reserved built-in tool name `{}`",
                    path.display(),
                    template.name
                );
            }
            log::warn!(
                "skipping auto-discovered tool template {}: name `{}` conflicts with built-in tool `{}`",
                path.display(),
                template.name,
                template.name
            );
            continue;
        }

        if let Some(first_path) = names.insert(template.name.clone(), path.clone()) {
            anyhow::bail!(
                "duplicate tool template name `{}` in {} and {}",
                template.name,
                first_path.display(),
                path.display()
            );
        }
        loaded.push(DiscoveredTemplate {
            template,
            path: path.clone(),
            explicit,
        });
    }

    Ok(loaded)
}

fn load_template_file(path: &Path) -> Result<ToolTemplate> {
    let yaml = fs::read_to_string(path)
        .with_context(|| format!("failed to read tool template {}", path.display()))?;
    let template = parse_template_str(&yaml, path)?;
    validate_tool_name(&template.name)
        .with_context(|| format!("failed to load tool template {}", path.display()))?;
    Ok(template)
}

fn validate_tool_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    let valid = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_');
    if !valid {
        anyhow::bail!("invalid tool template name `{name}`: must match ^[a-zA-Z][a-zA-Z0-9_]*$");
    }
    Ok(())
}

fn merge_template_pass(
    discovered: &mut Vec<DiscoveredTemplate>,
    indexes: &mut HashMap<String, usize>,
    templates: Vec<DiscoveredTemplate>,
) -> Result<()> {
    for template in templates {
        if let Some(&index) = indexes.get(&template.template.name) {
            let previous = &discovered[index];
            if template.explicit && !previous.explicit {
                log::info!(
                    "explicit tool template {} shadows auto-discovered template {} for tool `{}`",
                    template.path.display(),
                    previous.path.display(),
                    template.template.name
                );
                discovered[index] = template;
                continue;
            }
            anyhow::bail!(
                "duplicate tool template name `{}` in {} and {}",
                template.template.name,
                previous.path.display(),
                template.path.display()
            );
        }

        indexes.insert(template.template.name.clone(), discovered.len());
        discovered.push(template);
    }
    Ok(())
}

pub(crate) fn parse_template_str(yaml: &str, source: &Path) -> Result<ToolTemplate> {
    let template: ToolTemplate = serde_yaml::from_str(yaml)
        .with_context(|| format!("failed to parse tool template {}", source.display()))?;
    template
        .compiled_patterns()
        .with_context(|| format!("failed to load tool template {}", source.display()))?;
    validate_env_name_uniqueness(&template)
        .with_context(|| format!("failed to load tool template {}", source.display()))?;
    template
        .input_schema()
        .with_context(|| format!("failed to load tool template {}", source.display()))?;
    Ok(template)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use serde_json::json;

    use super::{discover_templates, param_to_env_name, parse_template_str, ParamType};

    const CANONICAL_YAML: &str = r#"name: gh_issue_view
description: View a GitHub issue as JSON
parameters:
  number: { type: integer, required: true, description: Issue number }
  repo:   { type: string,  required: true, pattern: "^[\\w.-]+/[\\w.-]+$" }
env: { GH_PAGER: "" }
sandbox:
  read:  ["~/.config/gh"]
  env:   ["GH_TOKEN"]
template: false
script: |
  set -euo pipefail
  gh issue view "$NUMBER" --repo "$REPO" --json title,body,comments
"#;

    const VALIDATION_YAML: &str = r#"name: validation
parameters:
  mode: { type: string, enum: [read, write] }
  ratio: { type: number, min: 0.5, max: 2.5 }
  enabled: { type: boolean }
  message: { type: string, min: 2, max: 20 }
  retries: { type: integer, default: 3 }
  note: { type: string }
script: echo ok
"#;

    fn write_template(path: &Path, name: &str, script: &str) {
        fs::write(path, format!("name: {name}\nscript: {script}\n"))
            .expect("template should be written");
    }

    #[test]
    fn command_templates_expose_reserved_timeout_control() {
        let template = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");
        let schema = template.input_schema().expect("schema should build");
        let timeout = &schema["properties"]["timeout_secs"];

        assert_eq!(timeout["type"], json!("integer"));
        assert_eq!(timeout["minimum"], json!(0));
        assert!(timeout["description"]
            .as_str()
            .is_some_and(|description| description.contains("86400")));
    }

    #[test]
    fn command_templates_reject_timeout_parameter_collision() {
        let error = parse_template_str(
            "name: collision\nparameters:\n  timeout_secs: { type: integer }\nscript: true\n",
            Path::new("collision.yaml"),
        )
        .expect_err("timeout_secs is reserved for execution control");

        assert!(format!("{error:#}").contains("TIMEOUT_SECS"), "{error:#}");
    }

    #[test]
    fn discovers_sorted_yaml_templates_from_package_dir() {
        let package = tempfile::tempdir().expect("package temp dir");
        let tools_dir = package.path().join("bash_tools");
        fs::create_dir(&tools_dir).expect("bash_tools dir should be created");
        write_template(&tools_dir.join("z.yaml"), "z_tool", "echo z");
        write_template(&tools_dir.join("a.yaml"), "a_tool", "echo a");
        fs::write(
            tools_dir.join("ignored.yml"),
            "name: ignored\nscript: echo ignored\n",
        )
        .expect("non-yaml file should be written");

        let templates = discover_templates(Some(package.path()), &[], &[])
            .expect("package templates should be discovered");

        assert_eq!(
            templates
                .iter()
                .map(|template| template.name.as_str())
                .collect::<Vec<_>>(),
            ["a_tool", "z_tool"]
        );
    }

    #[test]
    fn missing_package_bash_tools_directory_is_empty() {
        let package = tempfile::tempdir().expect("package temp dir");

        let templates = discover_templates(Some(package.path()), &[], &[])
            .expect("missing bash_tools directory should be allowed");

        assert!(templates.is_empty());
    }

    #[test]
    fn explicit_file_overrides_auto_discovered_template() {
        let package = tempfile::tempdir().expect("package temp dir");
        let tools_dir = package.path().join("bash_tools");
        fs::create_dir(&tools_dir).expect("bash_tools dir should be created");
        write_template(&tools_dir.join("auto.yaml"), "shared_tool", "echo auto");
        let explicit = package.path().join("explicit.yaml");
        write_template(&explicit, "shared_tool", "echo explicit");

        let templates = discover_templates(Some(package.path()), &[explicit], &[])
            .expect("explicit template should override auto template");

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "shared_tool");
        assert_eq!(templates[0].script, "echo explicit");
    }

    #[test]
    fn duplicate_names_in_one_directory_name_both_paths() {
        let explicit_dir = tempfile::tempdir().expect("tools temp dir");
        let first = explicit_dir.path().join("first.yaml");
        let second = explicit_dir.path().join("second.yaml");
        write_template(&first, "duplicate_tool", "echo first");
        write_template(&second, "duplicate_tool", "echo second");

        let error = discover_templates(None, &[], &[explicit_dir.path().to_path_buf()])
            .expect_err("duplicate tool names should fail");
        let message = format!("{error:#}");

        assert!(message.contains(&first.display().to_string()), "{message}");
        assert!(message.contains(&second.display().to_string()), "{message}");
    }

    #[test]
    fn malformed_auto_template_is_skipped_while_valid_template_loads() {
        let package = tempfile::tempdir().expect("package temp dir");
        let tools_dir = package.path().join("bash_tools");
        fs::create_dir(&tools_dir).expect("bash_tools dir should be created");
        fs::write(tools_dir.join("broken.yaml"), "name: [")
            .expect("broken template should be written");
        write_template(&tools_dir.join("valid.yaml"), "valid_tool", "echo valid");

        let templates = discover_templates(Some(package.path()), &[], &[])
            .expect("invalid auto template should be skipped");

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "valid_tool");
    }

    #[test]
    fn malformed_explicit_file_is_a_hard_error() {
        let explicit_dir = tempfile::tempdir().expect("tools temp dir");
        let broken = explicit_dir.path().join("broken.yaml");
        fs::write(&broken, "name: [").expect("broken template should be written");

        let error = discover_templates(None, std::slice::from_ref(&broken), &[])
            .expect_err("invalid explicit template should fail");

        assert!(
            format!("{error:#}").contains(&broken.display().to_string()),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn invalid_mcp_tool_name_is_rejected() {
        let explicit_dir = tempfile::tempdir().expect("tools temp dir");
        let invalid = explicit_dir.path().join("invalid.yaml");
        write_template(&invalid, "invalid-name", "echo invalid");

        let error = discover_templates(None, std::slice::from_ref(&invalid), &[])
            .expect_err("invalid tool name should fail");
        let message = format!("{error:#}");

        assert!(
            message.contains(&invalid.display().to_string()),
            "{message}"
        );
        assert!(message.contains("^[a-zA-Z][a-zA-Z0-9_]*$"), "{message}");
    }

    #[test]
    fn invalid_parameter_env_name_is_rejected_during_discovery() {
        let explicit_dir = tempfile::tempdir().expect("tools temp dir");
        let invalid = explicit_dir.path().join("invalid-param.yaml");
        fs::write(
            &invalid,
            "name: invalid_param\nparameters:\n  PATH: { type: string }\nscript: echo invalid\n",
        )
        .expect("invalid parameter template should be written");

        let error = discover_templates(None, &[invalid], &[])
            .expect_err("critical environment variable should fail discovery");

        assert!(format!("{error:#}").contains("critical environment variable"));
    }

    #[test]
    fn colliding_parameter_env_names_are_rejected_during_discovery() {
        let explicit_dir = tempfile::tempdir().expect("tools temp dir");
        let invalid = explicit_dir.path().join("colliding-params.yaml");
        fs::write(
            &invalid,
            "name: colliding_params\nparameters:\n  a-b: { type: string }\n  a_b: { type: string }\nscript: echo invalid\n",
        )
        .expect("colliding parameter template should be written");

        let error = discover_templates(None, std::slice::from_ref(&invalid), &[])
            .expect_err("colliding parameter environment names should fail discovery");
        let message = format!("{error:#}");

        assert!(message.contains("`a-b`"), "{message}");
        assert!(message.contains("`a_b`"), "{message}");
        assert!(message.contains("`A_B`"), "{message}");
    }

    #[test]
    fn parameter_env_name_colliding_with_top_level_env_is_rejected_during_discovery() {
        let explicit_dir = tempfile::tempdir().expect("tools temp dir");
        let invalid = explicit_dir.path().join("colliding-top-level-env.yaml");
        fs::write(
            &invalid,
            "name: colliding_env\nparameters:\n  foo: { type: string }\nenv: { FOO: x }\nscript: echo invalid\n",
        )
        .expect("top-level environment collision template should be written");

        let error = discover_templates(None, std::slice::from_ref(&invalid), &[])
            .expect_err("parameter and top-level environment collision should fail discovery");
        let message = format!("{error:#}");

        assert!(message.contains("parameter `foo`"), "{message}");
        assert!(message.contains("`FOO`"), "{message}");
        assert!(message.contains("top-level `env:` entry"), "{message}");
    }

    #[test]
    fn noncolliding_parameter_and_top_level_env_names_load() {
        let explicit_dir = tempfile::tempdir().expect("tools temp dir");
        let valid = explicit_dir.path().join("gh-issue-view.yaml");
        fs::write(&valid, CANONICAL_YAML).expect("canonical template should be written");

        let templates = discover_templates(None, std::slice::from_ref(&valid), &[])
            .expect("noncolliding environment names should load");

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "gh_issue_view");
    }

    #[test]
    fn shipped_gh_issue_view_template_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/pantheon/bash_tools/gh_issue_view.yaml");

        let templates = discover_templates(None, &[path], &[])
            .expect("shipped gh_issue_view template should load");

        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "gh_issue_view");
    }

    #[test]
    fn non_json_enum_member_returns_load_error_instead_of_panicking() {
        let error = parse_template_str(
            "name: exotic\nparameters:\n  value:\n    type: string\n    enum:\n      - ? [a, b]\n        : value\nscript: echo ok\n",
            Path::new("exotic.yaml"),
        )
        .expect_err("non-string map key should fail template loading");
        let message = format!("{error:#}");

        assert!(message.contains("exotic.yaml"), "{message}");
        assert!(message.contains("not JSON-compatible"), "{message}");
    }

    #[test]
    fn render_script_returns_unrendered_body_when_templating_is_disabled() {
        let template = parse_template_str(
            "name: literal\nparameters:\n  repo: { type: string }\ntemplate: false\nscript: 'echo {{ repo }}'\n",
            Path::new("literal.yaml"),
        )
        .expect("literal template should parse");
        let bindings =
            std::collections::HashMap::from([("REPO".to_string(), "dobesv/harnx".to_string())]);

        assert_eq!(
            template
                .render_script(&bindings)
                .expect("disabled templating should return the script"),
            "echo {{ repo }}"
        );
    }

    #[test]
    fn render_script_conditionally_uses_only_bound_parameters() {
        let template = parse_template_str(
            "name: conditional\nparameters:\n  verbose: { type: boolean }\ntemplate: true\nscript: '{% if verbose is defined %}--verbose{% endif %}'\n",
            Path::new("conditional.yaml"),
        )
        .expect("conditional template should parse");
        let empty_args = json!({});
        let present_args = json!({ "verbose": true });
        let absent = template
            .validate_and_bind(
                empty_args
                    .as_object()
                    .expect("arguments should be an object"),
            )
            .expect("absent optional parameter should validate");
        let present = template
            .validate_and_bind(
                present_args
                    .as_object()
                    .expect("arguments should be an object"),
            )
            .expect("present optional parameter should validate");

        assert_eq!(template.render_script(&absent).unwrap(), "");
        assert_eq!(template.render_script(&present).unwrap(), "--verbose");
    }

    #[test]
    fn render_script_quote_filter_shell_escapes_single_quotes() {
        let template = parse_template_str(
            "name: quoted\nparameters:\n  repo: { type: string, required: true }\ntemplate: true\nscript: '{{ repo | quote }}'\n",
            Path::new("quoted.yaml"),
        )
        .expect("quoted template should parse");
        let args = json!({ "repo": "a'b" });
        let bindings = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect("repo should validate");

        assert_eq!(template.render_script(&bindings).unwrap(), r#"'a'\''b'"#);
    }

    #[test]
    fn render_script_context_uses_original_parameter_names() {
        let template = parse_template_str(
            "name: context\nparameters:\n  repo: { type: string, required: true }\ntemplate: true\nscript: '{{ repo }}|{{ REPO }}'\n",
            Path::new("context.yaml"),
        )
        .expect("context template should parse");
        let args = json!({ "repo": "dobesv/harnx" });
        let bindings = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect("repo should validate");

        assert_eq!(template.render_script(&bindings).unwrap(), "dobesv/harnx|");
    }

    #[test]
    fn parameter_names_transform_to_upper_snake_case() {
        assert_eq!(param_to_env_name("number").unwrap(), "NUMBER");
        assert_eq!(param_to_env_name("some-value").unwrap(), "SOME_VALUE");
    }

    #[test]
    fn parameter_name_rejects_leading_digit() {
        let error = param_to_env_name("1abc").unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid parameter name `1abc`: transformed environment variable name `1ABC` must start with A-Z or underscore"
        );
    }

    #[test]
    fn parameter_name_rejects_critical_environment_variable() {
        let error = param_to_env_name("PATH").unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid parameter name `PATH`: transformed environment variable name `PATH` collides with a critical environment variable"
        );
    }

    #[test]
    fn parameter_name_rejects_bash_control_environment_variables() {
        for parameter in ["ps4", "bash_xtracefd", "cdpath", "globignore", "bashopts"] {
            let error = param_to_env_name(parameter).unwrap_err();
            let env_name = parameter.to_uppercase();

            assert!(
                error.to_string().contains(&format!(
                    "environment variable name `{env_name}` collides with a critical environment variable"
                )),
                "unexpected error for {parameter}: {error:#}"
            );
        }
    }

    #[test]
    fn parameter_name_rejects_shellshock_prefix() {
        let error = param_to_env_name("bash_func_x").unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid parameter name `bash_func_x`: transformed environment variable name `BASH_FUNC_X` uses reserved BASH_FUNC prefix"
        );
    }

    #[test]
    fn parameter_name_rejects_non_ascii_input() {
        let error = param_to_env_name("naïve").unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid parameter name `naïve`: must contain ASCII characters only"
        );
    }

    #[test]
    fn validate_and_bind_materializes_canonical_arguments() {
        let template = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");
        let args = json!({ "number": 42, "repo": "dobesv/harnx" });

        let bindings = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect("canonical arguments should validate");

        assert_eq!(
            bindings,
            std::collections::HashMap::from([
                ("NUMBER".to_string(), "42".to_string()),
                ("REPO".to_string(), "dobesv/harnx".to_string()),
            ])
        );
        assert_eq!(bindings["NUMBER"], "42");
        assert_ne!(bindings["NUMBER"], "42.0");
    }

    #[test]
    fn validate_and_bind_rejects_missing_required_parameter() {
        let template = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");
        let args = json!({ "repo": "dobesv/harnx" });

        let error = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect_err("missing required parameter should fail");

        assert_eq!(error.to_string(), "missing required parameter `number`");
    }

    #[test]
    fn validate_and_bind_rejects_wrong_type() {
        let template = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");
        let args = json!({ "number": "42", "repo": "dobesv/harnx" });

        let error = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect_err("string integer should fail");

        assert_eq!(
            error.to_string(),
            "invalid type for parameter `number`: expected integer, got string"
        );
    }

    #[test]
    fn validate_and_bind_rejects_fractional_integer() {
        let template = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");
        let args = json!({ "number": 42.5, "repo": "dobesv/harnx" });

        let error = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect_err("fractional integer should fail");

        assert_eq!(
            error.to_string(),
            "invalid type for parameter `number`: expected integer, got number"
        );
    }

    #[test]
    fn validate_and_bind_rejects_pattern_miss() {
        let template = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");
        let args = json!({ "number": 42, "repo": "bad" });

        let error = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect_err("pattern miss should fail");

        assert!(
            error.to_string().contains("parameter `repo`"),
            "unexpected error: {error:#}"
        );
        assert!(
            error.to_string().contains("does not match pattern"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn validate_and_bind_rejects_unknown_argument() {
        let template = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");
        let args = json!({
            "number": 42,
            "repo": "dobesv/harnx",
            "unexpected": true,
        });

        let error = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect_err("unknown argument should fail");

        assert_eq!(
            error.to_string(),
            "unknown argument `unexpected`: no parameter with that name"
        );
    }

    #[test]
    fn validate_and_bind_enforces_enum_and_numeric_range() {
        let template = parse_template_str(VALIDATION_YAML, Path::new("validation.yaml"))
            .expect("validation template should parse");

        let enum_args = json!({ "mode": "execute" });
        let enum_error = template
            .validate_and_bind(
                enum_args
                    .as_object()
                    .expect("enum arguments should be an object"),
            )
            .expect_err("enum violation should fail");
        assert!(
            enum_error.to_string().contains("parameter `mode`"),
            "unexpected error: {enum_error:#}"
        );
        assert!(
            enum_error.to_string().contains("allowed enum values"),
            "unexpected error: {enum_error:#}"
        );

        let range_args = json!({ "ratio": 3.0 });
        let range_error = template
            .validate_and_bind(
                range_args
                    .as_object()
                    .expect("range arguments should be an object"),
            )
            .expect_err("out-of-range number should fail");
        assert_eq!(
            range_error.to_string(),
            "invalid value for parameter `ratio`: value must be at most 2.5"
        );

        let string_range_args = json!({ "message": "é" });
        let string_range_error = template
            .validate_and_bind(
                string_range_args
                    .as_object()
                    .expect("string range arguments should be an object"),
            )
            .expect_err("short string should fail");
        assert_eq!(
            string_range_error.to_string(),
            "invalid value for parameter `message`: character length must be at least 2"
        );
    }

    #[test]
    fn validate_and_bind_materializes_defaults_and_skips_absent_optionals() {
        let template = parse_template_str(VALIDATION_YAML, Path::new("validation.yaml"))
            .expect("validation template should parse");
        let args = json!({});

        let bindings = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect("defaults should validate");

        assert_eq!(
            bindings,
            std::collections::HashMap::from([("RETRIES".to_string(), "3".to_string())])
        );
        assert!(!bindings.contains_key("NOTE"));
    }

    #[test]
    fn validate_and_bind_preserves_shell_metacharacters_as_literal_env_value() {
        let template = parse_template_str(VALIDATION_YAML, Path::new("validation.yaml"))
            .expect("validation template should parse");
        let payload = r#""; rm -rf /""#;
        let args = json!({ "message": payload });

        let bindings = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect("shell metacharacters are valid inside an environment value");

        assert_eq!(bindings["MESSAGE"], payload);
    }

    #[test]
    fn validate_and_bind_materializes_number_and_boolean_values() {
        let template = parse_template_str(VALIDATION_YAML, Path::new("validation.yaml"))
            .expect("validation template should parse");
        let args = json!({ "ratio": 1.25, "enabled": true });

        let bindings = template
            .validate_and_bind(args.as_object().expect("arguments should be an object"))
            .expect("number and boolean arguments should validate");

        assert_eq!(bindings["RATIO"], "1.25");
        assert_eq!(bindings["ENABLED"], "true");
    }

    #[test]
    fn parses_canonical_tool_template() {
        let parsed = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");

        assert_eq!(parsed.name, "gh_issue_view");
        assert_eq!(
            parsed.description.as_deref(),
            Some("View a GitHub issue as JSON")
        );

        let number = parsed
            .parameters
            .get("number")
            .expect("number parameter should exist");
        assert!(matches!(number.ty, ParamType::Integer));
        assert!(number.required);
        assert_eq!(number.description.as_deref(), Some("Issue number"));

        let repo = parsed
            .parameters
            .get("repo")
            .expect("repo parameter should exist");
        assert!(matches!(repo.ty, ParamType::String));
        assert!(repo.required);
        assert_eq!(repo.pattern.as_deref(), Some(r"^[\w.-]+/[\w.-]+$"));

        assert_eq!(
            parsed.input_schema().expect("schema should build"),
            json!({
                "type": "object",
                "properties": {
                    "number": {
                        "type": "integer",
                        "description": "Issue number",
                    },
                    "repo": {
                        "type": "string",
                        "pattern": r"^[\w.-]+/[\w.-]+$",
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Kill the command after this many seconds. Defaults to 86400 (24 hours); use 0 for no deadline.",
                    },
                },
                "required": ["number", "repo"],
                "additionalProperties": false,
            })
        );

        assert_eq!(
            parsed.env.as_ref().and_then(|env| env.get("GH_PAGER")),
            Some(&String::new())
        );

        let sandbox = parsed.sandbox.expect("sandbox should exist");
        assert!(sandbox.enabled);
        assert!(sandbox.network);
        assert_eq!(sandbox.read, ["~/.config/gh"]);
        assert!(sandbox.write.is_empty());
        assert_eq!(sandbox.env, ["GH_TOKEN"]);

        assert!(!parsed.template);
        assert_eq!(
            parsed.script,
            "set -euo pipefail\ngh issue view \"$NUMBER\" --repo \"$REPO\" --json title,body,comments\n"
        );
    }

    #[test]
    fn input_schema_maps_parameter_types_and_constraints() {
        let parsed = parse_template_str(
            r#"name: constrained
parameters:
  branch:
    type: string
    required: true
    description: Git branch name
    pattern: "^[a-z]+$"
    enum: [main, 7, true]
    min: 2.9
    max: 12.8
  attempts: { type: integer, required: true, min: 1, max: 5 }
  ratio: { type: number, min: 0.25, max: 1.5 }
  dry-run: { type: boolean }
script: echo ok
"#,
            Path::new("constrained.yaml"),
        )
        .expect("constrained template should parse");

        assert_eq!(
            parsed.input_schema().expect("schema should build"),
            json!({
                "type": "object",
                "properties": {
                    "attempts": {
                        "type": "integer",
                        "minimum": 1.0,
                        "maximum": 5.0,
                    },
                    "branch": {
                        "type": "string",
                        "description": "Git branch name",
                        "enum": ["main", 7, true],
                        "minLength": 3,
                        "maxLength": 12,
                        "pattern": "^[a-z]+$",
                    },
                    "dry-run": { "type": "boolean" },
                    "ratio": {
                        "type": "number",
                        "minimum": 0.25,
                        "maximum": 1.5,
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Kill the command after this many seconds. Defaults to 86400 (24 hours); use 0 for no deadline.",
                    },
                },
                "required": ["attempts", "branch"],
                "additionalProperties": false,
            })
        );
    }

    #[test]
    fn compiled_patterns_are_keyed_by_parameter_name() {
        let parsed = parse_template_str(CANONICAL_YAML, Path::new("gh_issue_view.yaml"))
            .expect("canonical template should parse");

        let patterns = parsed
            .compiled_patterns()
            .expect("canonical patterns should compile");

        assert_eq!(patterns.len(), 1);
        assert!(patterns["repo"].is_match("owner/repo"));
        assert!(!patterns["repo"].is_match("missing-slash"));
    }

    #[test]
    fn template_load_rejects_invalid_regex_with_parameter_name() {
        let error = parse_template_str(
            "name: invalid-pattern\nparameters:\n  branch: { type: string, pattern: '(?<=main)-branch' }\nscript: echo ok\n",
            Path::new("invalid-pattern.yaml"),
        )
        .expect_err("lookbehind should fail template loading");
        let message = format!("{error:#}");

        assert!(
            message.contains("invalid-pattern.yaml"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("invalid pattern for parameter `branch`"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn empty_sandbox_uses_defaults() {
        let parsed = parse_template_str(
            "name: defaults\nsandbox: {}\nscript: echo ok\n",
            Path::new("defaults.yaml"),
        )
        .expect("empty sandbox should parse");
        let sandbox = parsed.sandbox.expect("sandbox should exist");

        assert!(sandbox.enabled);
        assert!(sandbox.network);
        assert!(sandbox.read.is_empty());
        assert!(sandbox.write.is_empty());
        assert!(sandbox.env.is_empty());
    }

    #[test]
    fn malformed_yaml_error_includes_source_path() {
        let source = Path::new("templates/broken.yaml");
        let error = parse_template_str("name: [", source).expect_err("YAML should be invalid");

        assert!(
            error.to_string().contains("templates/broken.yaml"),
            "unexpected error: {error:#}"
        );
    }
}
