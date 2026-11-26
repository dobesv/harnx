//! Each file parsed with the type the runtime reads it into. A file that
//! doesn't parse is a `parse-error` with serde's line; every key the type
//! ignores is an `unknown-key`, located by its dotted path and with a
//! suggestion when a field at the same level is spelled close to it.
//!
//! `serde_ignored` sees a key only when serde hands it to a struct field by
//! field. Values serde buffers first are invisible to it: `flatten`,
//! untagged and internally tagged enums, and types with a hand-written
//! `Deserialize` that goes through `serde_json::Value`. The call sites below
//! name each such gap.

use std::path::{Path, PathBuf};

use harnx_core::access_rules::AccessRules;
use harnx_core::agent_config::{agent_front_matter, AgentFrontMatter};
use harnx_core::config_data::ConfigData;
use harnx_core::package::{PackageManifest, PackageMetadata, PackagePatch};
use harnx_core::script_tool::ScriptToolDefinition;
use harnx_core::user_aliases::UserAliases;
use serde::de::DeserializeOwned;
use serde_yaml::Value;

use super::field_probe::{fields_at, FieldProbe};
use super::{CheckContext, Code, ConfigLayout, Finding, PackageLayout, Report};
use crate::client::ClientConfig;
use crate::config::{Macro, ModelsOverride, NatsServerConfig, ToolServerConfig};

/// The largest edit distance at which an unknown key gets a suggestion.
const MAX_SUGGESTION_DISTANCE: usize = 2;

/// Parse every file the layout lists, counting each in
/// `summary.files_checked`. `--access-rules` and `--users` are already in the
/// layout in place of the directory's own files.
pub fn check_files(context: &CheckContext<'_>, report: &mut Report) {
    let layout = context.layout;
    for (path, check) in file_checks(layout) {
        let file = layout.relative(path);
        let findings = match std::fs::read_to_string(path) {
            Ok(content) => check(&file, &content),
            Err(error) => {
                vec![Finding::of(Code::ParseError, format!("cannot read: {error}")).in_file(file)]
            }
        };
        report.summary.files_checked += 1;
        for finding in findings {
            report.push(finding);
        }
    }
}

/// Check one file's content; `file` is the path findings report.
type FileCheck = fn(&Path, &str) -> Vec<Finding>;

/// Every file in the layout with the check for its kind.
fn file_checks(layout: &ConfigLayout) -> Vec<(&Path, FileCheck)> {
    let mut checks = Vec::new();
    // `script_tools:`, `hooks:` and `middleware:` entries go through raw
    // structs with `deny_unknown_fields` behind a `serde_json::Value`, so
    // serde_ignored can't see into them; their own unknown keys come back
    // as parse errors instead.
    add(&mut checks, &layout.config_file, typed::<ConfigData>);
    add(&mut checks, &layout.clients, client);
    add(&mut checks, &layout.tool_servers, typed::<ToolServerConfig>);
    add(&mut checks, &layout.nats_servers, typed::<NatsServerConfig>);
    add(&mut checks, &layout.agents, agent);
    add(&mut checks, &layout.macros, typed::<Macro>);
    add(
        &mut checks,
        &layout.models_override,
        typed::<ModelsOverride>,
    );
    add(&mut checks, &layout.access, access_rules);
    add(&mut checks, &layout.users, user_aliases);
    for package in &layout.packages {
        add_package(&mut checks, package);
    }
    checks
}

fn add_package<'a>(checks: &mut Vec<(&'a Path, FileCheck)>, package: &'a PackageLayout) {
    add(checks, &package.patch_file, typed::<PackagePatch>);
    add(checks, &package.clients, client);
    add(checks, &package.tool_servers, typed::<ToolServerConfig>);
    add(checks, &package.agents, agent);
    // Script tools decode through a raw struct with `deny_unknown_fields`;
    // its `flatten`ed source is serialize-only. Unknown keys are parse errors.
    add(checks, &package.script_tools, typed::<ScriptToolDefinition>);
    // The template type lives in harnx-bash-tools, which harnx-runtime
    // doesn't depend on: check the YAML syntax only.
    add(checks, &package.bash_tools, yaml_syntax);
    // `source:` is an internally tagged enum, buffered before its variant is
    // picked: unknown keys inside it aren't seen.
    add(checks, &package.manifest, typed::<PackageManifest>);
    add(checks, &package.metadata, typed::<PackageMetadata>);
}

fn add<'a>(
    checks: &mut Vec<(&'a Path, FileCheck)>,
    files: impl IntoIterator<Item = &'a PathBuf>,
    check: FileCheck,
) {
    checks.extend(files.into_iter().map(|file| (file.as_path(), check)));
}

/// Deserialize `content` as `T`, collecting the dotted path of every key `T`
/// ignores. A parse error becomes a `parse-error` finding with its line.
pub fn parse_yaml<T: DeserializeOwned>(
    file: &Path,
    content: &str,
) -> Result<(T, Vec<String>), Finding> {
    let mut unknown = Vec::new();
    let value = serde_ignored::deserialize(serde_yaml::Deserializer::from_str(content), |path| {
        unknown.push(key_path(&path))
    })
    .map_err(|error| yaml_error(file, &error))?;
    Ok((value, unknown))
}

/// One `unknown-key` error per path in `unknown`, suggesting the closest of
/// `known`, the field names at that path's level, when one is close enough.
pub fn unknown_key_findings(file: &Path, unknown: &[String], known: &[&str]) -> Vec<Finding> {
    unknown
        .iter()
        .map(|path| {
            let key = path.rsplit('.').next().unwrap_or(path);
            let message = match closest(key, known) {
                Some(candidate) => format!("unknown key {path} (did you mean {candidate}?)"),
                None => format!("unknown key {path}"),
            };
            Finding::of(Code::UnknownKey, message)
                .in_file(file)
                .at_key(path.clone())
        })
        .collect()
}

/// Parse a client file the way the loader does, but straight into the
/// provider's struct, which the tagged `ClientConfig` enum would buffer out
/// of serde_ignored's sight. The `type` key itself is left out of the
/// unknown keys.
pub fn parse_client(file: &Path, content: &str) -> Result<(ClientConfig, Vec<String>), Finding> {
    let value: Value = serde_yaml::from_str(content).map_err(|error| yaml_error(file, &error))?;
    let type_name = client_type(&value)
        .map_err(|message| Finding::of(Code::ParseError, message).in_file(file))?;
    let mut unknown = Vec::new();
    let mut record = |path: serde_ignored::Path| unknown.push(key_path(&path));
    let deserializer =
        serde_ignored::Deserializer::new(serde_yaml::Deserializer::from_str(content), &mut record);
    match ClientConfig::deserialize_as(type_name, deserializer) {
        Some(Ok(config)) => {
            unknown.retain(|path| path != "type");
            Ok((config, unknown))
        }
        Some(Err(error)) => Err(yaml_error(file, &error)),
        None => Err(unknown_client_type(file, type_name)),
    }
}

fn client_type(value: &Value) -> Result<&str, &'static str> {
    match value.get("type") {
        Some(Value::String(name)) => Ok(name),
        Some(_) => Err("`type` must be a string"),
        None => Err("missing field `type`"),
    }
}

fn unknown_client_type(file: &Path, type_name: &str) -> Finding {
    let message = format!(
        "unknown client type {type_name}; harnx ignores this client (valid types: {})",
        ClientConfig::TYPE_NAMES.join(", ")
    );
    Finding::of(Code::UnknownClientType, message).in_file(file)
}

fn typed<T: DeserializeOwned>(file: &Path, content: &str) -> Vec<Finding> {
    match parse_yaml::<T>(file, content) {
        Ok((_, unknown)) => suggest_fields(file, &unknown, |probe| {
            let _ = T::deserialize(probe);
        }),
        Err(finding) => vec![finding],
    }
}

fn client(file: &Path, content: &str) -> Vec<Finding> {
    match parse_client(file, content) {
        Ok((config, unknown)) => suggest_fields(file, &unknown, |probe| {
            let _ = ClientConfig::deserialize_as(config.type_name(), probe);
        }),
        Err(finding) => vec![finding],
    }
}

/// An agent's front matter, with parse error lines counted from the top of
/// the file. A file without front matter is all prompt and has nothing to
/// check here.
fn agent(file: &Path, content: &str) -> Vec<Finding> {
    let Some((front_matter, offset)) = agent_front_matter(content) else {
        return Vec::new();
    };
    let lines_before = content[..offset].matches('\n').count();
    typed::<AgentFrontMatter>(file, front_matter)
        .into_iter()
        .map(|mut finding| {
            finding.line = finding.line.map(|line| line + lines_before);
            finding
        })
        .collect()
}

fn yaml_syntax(file: &Path, content: &str) -> Vec<Finding> {
    match serde_yaml::from_str::<Value>(content) {
        Ok(_) => Vec::new(),
        Err(error) => vec![yaml_error(file, &error)],
    }
}

/// `AccessRules` rejects unknown keys and invalid rules itself.
fn access_rules(file: &Path, content: &str) -> Vec<Finding> {
    AccessRules::from_yaml(content)
        .err()
        .map(|error| loader_error(file, &error))
        .into_iter()
        .collect()
}

/// `UserAliases` rejects unknown keys and invalid entries itself.
fn user_aliases(file: &Path, content: &str) -> Vec<Finding> {
    UserAliases::from_yaml(content)
        .err()
        .map(|error| loader_error(file, &error))
        .into_iter()
        .collect()
}

/// A loader's error chain as a `parse-error`. A YAML error in the chain
/// ends with its line and column already.
fn loader_error(file: &Path, error: &anyhow::Error) -> Finding {
    Finding::of(Code::ParseError, format!("{error:#}")).in_file(file)
}

/// serde_yaml's message ends with the line and column; the finding carries
/// the line on its own, so the message drops them.
fn yaml_error(file: &Path, error: &serde_yaml::Error) -> Finding {
    let message = error.to_string();
    let Some(location) = error.location() else {
        return Finding::of(Code::ParseError, message).in_file(file);
    };
    let suffix = format!(" at line {} column {}", location.line(), location.column());
    let message = message.strip_suffix(&suffix).unwrap_or(&message);
    Finding::of(Code::ParseError, message)
        .in_file(file)
        .at_line(location.line())
}

/// Unknown-key findings for `unknown`, each suggesting from the field names
/// `deserialize` asks for at its level.
fn suggest_fields(
    file: &Path,
    unknown: &[String],
    deserialize: impl Fn(FieldProbe<'_>),
) -> Vec<Finding> {
    unknown
        .iter()
        .flat_map(|path| {
            let segments: Vec<&str> = path.split('.').collect();
            let known = fields_at(&segments[..segments.len() - 1], &deserialize);
            unknown_key_findings(file, std::slice::from_ref(path), known)
        })
        .collect()
}

fn closest<'k>(key: &str, known: &[&'k str]) -> Option<&'k str> {
    known
        .iter()
        .map(|candidate| (strsim::damerau_levenshtein(key, candidate), *candidate))
        .filter(|(distance, _)| *distance <= MAX_SUGGESTION_DISTANCE)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// `a.0.b`: map keys and sequence indexes joined by dots. Unlike
/// serde_ignored's own rendering, an `Option` or newtype adds no segment.
fn key_path(path: &serde_ignored::Path<'_>) -> String {
    let mut segments = Vec::new();
    push_segments(path, &mut segments);
    segments.join(".")
}

fn push_segments(path: &serde_ignored::Path<'_>, segments: &mut Vec<String>) {
    use serde_ignored::Path;
    match path {
        Path::Root => {}
        Path::Seq { parent, index } => {
            push_segments(parent, segments);
            segments.push(index.to_string());
        }
        Path::Map { parent, key } => {
            push_segments(parent, segments);
            segments.push(key.clone());
        }
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => push_segments(parent, segments),
    }
}
