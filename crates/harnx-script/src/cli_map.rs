//! Schema-derived long flags, without shell tokenization or implicit stdin.
//!
//! `-o json|text` belongs to the command wrapper. Call [`split_output_args`]
//! before [`parse_args`]; the schema parser rejects short options itself.

use std::{collections::BTreeMap, error::Error, fmt};

use serde_json::{Map, Value};

use crate::schema::{flag_table, kebab_case, properties, FlagKind, Property, Scalar};

mod validation;
use validation::{parse_scalar, validate};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError {
    pub message: String,
}

impl UsageError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for UsageError {}

mod output;
pub use output::{split_output_args, OutputFormat};

/// Parse schema properties into an object with their ORIGINAL keys.
///
/// `--json` supplies a base object; flags override it regardless of ordering.
/// Repeated scalar-array flags replace the base array, then append in argv order.
/// Last scalar flag / last `--json` wins. Defaults are documented, not injected.
/// Stdin is ignored unless an explicit `--json -` requests it. There are no
/// positional arguments: `--` ends options, and any subsequent token is an error.
/// Numbers use JSON numeric syntax; integers require i64/u64 syntax without a
/// decimal point or exponent. Type and enum mismatches are usage errors, including
/// in the final merged base object. Complex/nullable/reference schemas use a JSON
/// flag rather than guessing a branch. Full schema validation (refs, unions,
/// nested object constraints, ranges, formats, etc.) remains the provider's job.
pub fn parse_args(
    schema: &Value,
    argv: &[String],
    stdin: Option<&str>,
) -> Result<Value, UsageError> {
    let properties = properties(schema);
    let mut base = Map::new();
    let mut overrides = Map::new();
    let mut flags = None;
    let mut index = 0;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            if let Some(value) = argv.get(index + 1) {
                return Err(positional_error(value));
            }
            break;
        }
        let option = arg.strip_prefix("--").ok_or_else(|| {
            if arg == "-o" || arg.starts_with("-o=") {
                UsageError::new(
                    "-o is a rendering option; call split_output_args before parse_args",
                )
            } else {
                positional_error(arg)
            }
        })?;
        let (name, inline) = option
            .split_once('=')
            .map_or((option, None), |(k, v)| (k, Some(v)));
        if name == "json" {
            let value = option_value(argv, &mut index, inline, name)?;
            base = json_base(value, stdin)?;
        } else {
            if flags.is_none() {
                flags = Some(flag_table(&properties)?);
            }
            let table = flags.as_ref().unwrap();
            let &(property, negated) = table.get(name).ok_or_else(|| unknown_flag(name, table))?;
            apply_flag(
                &properties[property],
                negated,
                inline,
                argv,
                &mut index,
                &mut overrides,
            )?;
        }
        index += 1;
    }
    base.extend(overrides);
    check_required(schema, &base)?;
    for property in &properties {
        if let Some(value) = base.get(property.key) {
            validate(property.schema, value)
                .map_err(|reason| UsageError::new(format!("--{}: {reason}", property.flag)))?;
        }
    }
    Ok(Value::Object(base))
}

fn positional_error(value: &str) -> UsageError {
    UsageError::new(format!(
        "unexpected argument {value:?}; use named --flags or --json (no positional arguments)"
    ))
}

fn option_value<'a>(
    argv: &'a [String],
    index: &mut usize,
    inline: Option<&'a str>,
    name: &str,
) -> Result<&'a str, UsageError> {
    if let Some(value) = inline {
        return Ok(value);
    }
    *index += 1;
    argv.get(*index)
        .filter(|value| !value.starts_with("--"))
        .map(String::as_str)
        .ok_or_else(|| {
            UsageError::new(format!(
                "--{name} requires a value; use --{name}=VALUE for a value starting with '--'"
            ))
        })
}

fn json_base(value: &str, stdin: Option<&str>) -> Result<Map<String, Value>, UsageError> {
    let value = if value == "-" {
        stdin.ok_or_else(|| UsageError::new("--json - requires stdin containing a JSON object"))?
    } else {
        value
    };
    match serde_json::from_str(value) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(UsageError::new("--json requires a JSON object")),
        Err(error) => Err(UsageError::new(format!("invalid --json object: {error}"))),
    }
}

fn apply_flag(
    property: &Property<'_>,
    negated: bool,
    inline: Option<&str>,
    argv: &[String],
    index: &mut usize,
    overrides: &mut Map<String, Value>,
) -> Result<(), UsageError> {
    let value = if property.kind == FlagKind::Scalar(Scalar::Boolean) {
        if inline.is_some() {
            return Err(UsageError::new(format!(
                "--{} is a switch; use --{} or --no-{} without a value",
                property.flag, property.flag, property.flag
            )));
        }
        Value::Bool(!negated)
    } else {
        let text = option_value(argv, index, inline, &property.flag)?;
        match property.kind {
            FlagKind::Scalar(kind) | FlagKind::Repeated(kind) => parse_scalar(text, kind),
            FlagKind::Json => {
                serde_json::from_str(text).map_err(|error| format!("expected JSON: {error}"))
            }
        }
        .map_err(|reason| UsageError::new(format!("--{}: {reason}", property.flag)))?
    };
    if matches!(property.kind, FlagKind::Repeated(_)) {
        overrides
            .entry(property.key.to_owned())
            .or_insert_with(|| Value::Array(vec![]))
            .as_array_mut()
            .unwrap()
            .push(value);
    } else {
        overrides.insert(property.key.to_owned(), value);
    }
    Ok(())
}

fn check_required(schema: &Value, values: &Map<String, Value>) -> Result<(), UsageError> {
    let missing: Vec<_> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|key| !values.contains_key(*key))
        .map(|key| format!("--{}", kebab_case(key)))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(UsageError::new(format!(
            "missing required flags: {} (or supply their original keys via --json)",
            missing.join(", ")
        )))
    }
}

fn unknown_flag(name: &str, table: &BTreeMap<String, (usize, bool)>) -> UsageError {
    let suggestion = table
        .keys()
        .map(String::as_str)
        .chain(std::iter::once("json"))
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= 2)
        .min();
    let hint = suggestion
        .map(|(_, flag)| format!("; did you mean '--{flag}'?"))
        .unwrap_or_default();
    UsageError::new(format!("unknown flag '--{name}'{hint}"))
}

pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<_> = b.chars().collect();
    let mut row: Vec<_> = (0..=b.len()).collect();
    for (i, left) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (row[j] + 1)
                .min(old + 1)
                .min(diagonal + usize::from(left != *right));
            diagonal = old;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests;
