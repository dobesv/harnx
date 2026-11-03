//! Shared flag names and type classification for parsing and documentation.

mod kebab;
pub(crate) use kebab::kebab_case;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::cli_map::UsageError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scalar {
    String,
    Number,
    Integer,
    Boolean,
}

impl Scalar {
    pub fn label(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlagKind {
    Scalar(Scalar),
    Repeated(Scalar),
    Json,
}

impl FlagKind {
    pub fn label(self) -> String {
        match self {
            Self::Scalar(scalar) => scalar.label().into(),
            Self::Repeated(scalar) => format!("{}[]", scalar.label()),
            Self::Json => "json".into(),
        }
    }
}

pub(crate) struct Property<'a> {
    pub key: &'a str,
    pub flag: String,
    pub schema: &'a Value,
    pub kind: FlagKind,
    pub required: bool,
}

impl Property<'_> {
    pub fn option(&self) -> String {
        match self.kind {
            FlagKind::Scalar(Scalar::Boolean) => format!("--{} / --no-{}", self.flag, self.flag),
            FlagKind::Repeated(kind) => format!("--{} <{}>", self.flag, kind.label()),
            _ => format!("--{} <{}>", self.flag, self.kind.label()),
        }
    }
}

pub(crate) fn is_complex(schema: &Value) -> bool {
    // A type beside $ref/anyOf can be an intersection, not the whole schema.
    // Don't guess which branch the provider expects: use JSON in these cases.
    schema.get("nullable").and_then(Value::as_bool) == Some(true)
        || [
            "$ref",
            "anyOf",
            "oneOf",
            "allOf",
            "not",
            "if",
            "then",
            "else",
            "prefixItems",
        ]
        .iter()
        .any(|key| schema.get(key).is_some())
}

pub(crate) fn scalar(schema: &Value) -> Option<Scalar> {
    if is_complex(schema) {
        return None;
    }
    match schema.get("type").and_then(Value::as_str)? {
        "string" => Some(Scalar::String),
        "number" => Some(Scalar::Number),
        "integer" => Some(Scalar::Integer),
        "boolean" => Some(Scalar::Boolean),
        _ => None,
    }
}

fn flag_kind(schema: &Value) -> FlagKind {
    if let Some(scalar) = scalar(schema) {
        return FlagKind::Scalar(scalar);
    }
    if schema.get("type").and_then(Value::as_str) == Some("array") && !is_complex(schema) {
        if let Some(item) = schema.get("items").and_then(scalar) {
            return FlagKind::Repeated(item);
        }
    }
    FlagKind::Json
}

/// Preserve original property keys separately; flag spelling is never reversed
/// to recover a key (camelCase and snake_case would otherwise be ambiguous).
pub(crate) fn properties(schema: &Value) -> Vec<Property<'_>> {
    let required = schema.get("required").and_then(Value::as_array);
    let mut result: Vec<_> = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(key, value)| Property {
            key,
            flag: kebab_case(key),
            schema: value,
            kind: flag_kind(value),
            required: required
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(key))),
        })
        .collect();
    result.sort_by(|a, b| a.flag.cmp(&b.flag).then(a.key.cmp(b.key)));
    result
}

/// Validates ambiguous spellings before any values can reach a tool. `--json`
/// remains an escape hatch for properties that cannot be represented as flags.
pub(crate) fn flag_table<'a>(
    properties: &[Property<'a>],
) -> Result<BTreeMap<String, (usize, bool)>, UsageError> {
    let mut flags = BTreeMap::new();
    for (index, property) in properties.iter().enumerate() {
        if property.flag == "json" || property.flag.is_empty() {
            continue;
        }
        insert_flag(&mut flags, property.flag.clone(), index, false)?;
        if property.kind == FlagKind::Scalar(Scalar::Boolean) {
            insert_flag(&mut flags, format!("no-{}", property.flag), index, true)?;
        }
    }
    Ok(flags)
}

fn insert_flag(
    flags: &mut BTreeMap<String, (usize, bool)>,
    name: String,
    index: usize,
    negated: bool,
) -> Result<(), UsageError> {
    if flags.insert(name.clone(), (index, negated)).is_some() {
        return Err(UsageError::new(format!(
            "schema has conflicting flag --{name}; supply arguments with --json instead"
        )));
    }
    Ok(())
}
