use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use jaq_all::{fmts, json};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadSpec {
    pub name: String,
    pub path: PathBuf,
    pub format: LoadFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadFormat {
    Yaml,
    Json,
    Raw,
}

pub fn parse_load_specs(yaml: &[String], json: &[String], raw: &[String]) -> Result<Vec<LoadSpec>> {
    let mut specs = Vec::new();

    for entry in yaml {
        specs.push(parse_load_spec(entry, LoadFormat::Yaml)?);
    }
    for entry in json {
        specs.push(parse_load_spec(entry, LoadFormat::Json)?);
    }
    for entry in raw {
        specs.push(parse_load_spec(entry, LoadFormat::Raw)?);
    }

    ensure_unique_names(&specs)?;
    Ok(specs)
}

pub fn load_value(spec: &LoadSpec) -> json::Val {
    match load_json_value(spec) {
        Ok(value) => match serde_json_to_jaq_value(&value) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(name = %spec.name, path = %spec.path.display(), format = ?spec.format, error = %error, "failed to convert loaded jaq variable; using null");
                json::Val::Null
            }
        },
        Err(LoadError::MissingOrUnreadable) => json::Val::Null,
        Err(LoadError::Malformed(message)) => {
            tracing::warn!(name = %spec.name, path = %spec.path.display(), format = ?spec.format, error = %message, "failed to parse loaded jaq variable; using null");
            json::Val::Null
        }
    }
}

fn parse_load_spec(input: &str, format: LoadFormat) -> Result<LoadSpec> {
    let (name, path) = input
        .split_once('=')
        .ok_or_else(|| anyhow!("load flag must be in <name>=<path> form: {input}"))?;

    validate_var_name(name)?;

    Ok(LoadSpec {
        name: name.to_owned(),
        path: expand_home(path),
        format,
    })
}

const RESERVED_VAR_NAMES: &[&str] = &[
    "fake_uuid_key",
    "fake_base64_key",
    "fake_url_base64_key",
    "fake_hex_key",
    "fake_email",
    "temp_file_root",
];

pub fn ensure_valid_loaded_var_name(name: &str) -> Result<()> {
    validate_var_name(name)?;
    if RESERVED_VAR_NAMES.contains(&name) {
        bail!("loaded jaq variable name is reserved: {name}");
    }
    Ok(())
}

pub fn ensure_unique_loaded_var_names<I>(names: I) -> Result<()>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        let name = name.as_ref();
        ensure_valid_loaded_var_name(name)?;
        if !seen.insert(name.to_owned()) {
            bail!("duplicate loaded jaq variable name: {name}");
        }
    }
    Ok(())
}

fn ensure_unique_names(specs: &[LoadSpec]) -> Result<()> {
    ensure_unique_loaded_var_names(specs.iter().map(|spec| spec.name.as_str()))
}

pub fn validate_var_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("invalid jaq variable name: empty");
    };

    if !(first.is_ascii_alphabetic() || first == '_') {
        bail!("invalid jaq variable name `{name}`: must match [A-Za-z_][A-Za-z0-9_]*");
    }

    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        bail!("invalid jaq variable name `{name}`: must match [A-Za-z_][A-Za-z0-9_]*");
    }

    Ok(())
}

pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }

    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }

    PathBuf::from(path)
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn load_json_value(spec: &LoadSpec) -> std::result::Result<Value, LoadError> {
    let contents = match fs::read_to_string(&spec.path) {
        Ok(contents) => contents,
        Err(_) => return Err(LoadError::MissingOrUnreadable),
    };

    match spec.format {
        LoadFormat::Yaml => serde_yaml::from_str::<Value>(&contents)
            .map_err(|error| LoadError::Malformed(error.to_string())),
        LoadFormat::Json => serde_json::from_str::<Value>(&contents)
            .map_err(|error| LoadError::Malformed(error.to_string())),
        LoadFormat::Raw => Ok(Value::String(contents)),
    }
}

pub fn serde_json_to_jaq_value(value: &Value) -> Result<json::Val> {
    fmts::read::json::parse_single(value.to_string().as_bytes())
        .with_context(|| "serde_json to jaq conversion")
}

#[derive(Debug)]
enum LoadError {
    MissingOrUnreadable,
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::{
        expand_home, load_value, parse_load_specs, validate_var_name, LoadFormat, LoadSpec,
    };
    use tempfile::tempdir;

    #[test]
    fn parse_load_specs_rejects_invalid_var_name() {
        let err = parse_load_specs(&["1bad=/tmp/x".into()], &[], &[]).expect_err("invalid name");
        assert!(err.to_string().contains("invalid jaq variable name"));
    }

    #[test]
    fn parse_load_specs_rejects_reserved_name() {
        let err = parse_load_specs(&["fake_uuid_key=/tmp/a".into()], &[], &[])
            .expect_err("reserved name");
        assert!(err.to_string().contains("reserved"));

        let err = parse_load_specs(&[], &[], &["temp_file_root=/tmp/a".into()])
            .expect_err("reserved name");
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn parse_load_specs_rejects_duplicate_name() {
        let err = parse_load_specs(&["cfg=/tmp/a".into()], &["cfg=/tmp/b".into()], &[])
            .expect_err("duplicate name");
        assert!(err
            .to_string()
            .contains("duplicate loaded jaq variable name"));
    }

    #[test]
    fn expand_home_expands_tilde_prefix() {
        let home = std::env::var("HOME").expect("HOME set in test env");
        assert_eq!(
            expand_home("~/cfg.yaml"),
            std::path::PathBuf::from(home).join("cfg.yaml")
        );
    }

    #[test]
    fn load_yaml_parses_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.yaml");
        std::fs::write(&path, "root:\n  child: value\n").unwrap();
        let value = load_value(&LoadSpec {
            name: "cfg".into(),
            path,
            format: LoadFormat::Yaml,
        });
        assert_eq!(value.to_string(), "{\"root\":{\"child\":\"value\"}}");
    }

    #[test]
    fn load_json_parses_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        std::fs::write(&path, "{\"root\": {\"child\": \"value\"}}\n").unwrap();
        let value = load_value(&LoadSpec {
            name: "cfg".into(),
            path,
            format: LoadFormat::Json,
        });
        assert_eq!(value.to_string(), "{\"root\":{\"child\":\"value\"}}");
    }

    #[test]
    fn load_raw_returns_string() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.txt");
        std::fs::write(&path, "hello\nworld").unwrap();
        let value = load_value(&LoadSpec {
            name: "cfg".into(),
            path,
            format: LoadFormat::Raw,
        });
        assert_eq!(value.to_string(), "\"hello\\nworld\"");
    }

    #[test]
    fn missing_file_becomes_null() {
        let dir = tempdir().unwrap();
        let value = load_value(&LoadSpec {
            name: "cfg".into(),
            path: dir.path().join("missing.yaml"),
            format: LoadFormat::Yaml,
        });
        assert_eq!(value, jaq_all::json::Val::Null);
    }

    #[test]
    fn malformed_file_becomes_null() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not-json").unwrap();
        let value = load_value(&LoadSpec {
            name: "cfg".into(),
            path,
            format: LoadFormat::Json,
        });
        assert_eq!(value, jaq_all::json::Val::Null);
    }

    #[test]
    fn validate_var_name_accepts_valid_names() {
        validate_var_name("cfg_1").unwrap();
    }

    #[test]
    fn malformed_yaml_becomes_null() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(
            &path, "root: [
",
        )
        .unwrap();
        let value = load_value(&LoadSpec {
            name: "cfg".into(),
            path,
            format: LoadFormat::Yaml,
        });
        assert_eq!(value, jaq_all::json::Val::Null);
    }
}
