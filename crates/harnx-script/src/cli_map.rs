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
        write!(f, "{}", self.message)
    }
}

impl Error for UsageError {}

pub use output::{split_output_args, OutputFormat};
mod output;

/// Convert a schema-constrained argv into a JSON argument object.
///
/// Top-level properties become `--kebab-name value` flags mapped back to their
/// original schema keys. Booleans produce `--flag` and `--no-flag` switches.
/// Repeated flags append to an array. Object, complex, or nullable properties
/// take a single JSON string argument: `--settings '{"indent": 2}'`.
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
    let mut parser = ArgParser::new(properties, stdin);
    let mut index = 0;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            check_end_of_options(argv, index)?;
            break;
        }
        let option = strip_flag_prefix(arg)?;
        let (name, inline) = split_flag(option);
        let source = ValueSource {
            argv,
            index: &mut index,
            inline,
        };
        parser.parse_flag(name, source)?;
        index += 1;
    }
    parser.finish(schema)
}

fn check_end_of_options(argv: &[String], index: usize) -> Result<(), UsageError> {
    if let Some(value) = argv.get(index + 1) {
        return Err(positional_error(value));
    }
    Ok(())
}

fn strip_flag_prefix(arg: &str) -> Result<&str, UsageError> {
    arg.strip_prefix("--").ok_or_else(|| {
        if arg == "-o" || arg.starts_with("-o=") {
            UsageError::new("-o is a rendering option; call split_output_args before parse_args")
        } else {
            positional_error(arg)
        }
    })
}

fn split_flag(option: &str) -> (&str, Option<&str>) {
    option
        .split_once('=')
        .map_or((option, None), |(k, v)| (k, Some(v)))
}

struct ValueSource<'a> {
    argv: &'a [String],
    index: &'a mut usize,
    inline: Option<&'a str>,
}

impl<'a> ValueSource<'a> {
    fn read_value(&mut self, name: &str) -> Result<&'a str, UsageError> {
        if let Some(value) = self.inline {
            return Ok(value);
        }
        *self.index += 1;
        self.argv
            .get(*self.index)
            .filter(|value| !value.starts_with("--"))
            .map(String::as_str)
            .ok_or_else(|| {
                UsageError::new(format!(
                    "--{name} requires a value; use --{name}=VALUE for a value starting with '--'"
                ))
            })
    }
}

struct ArgParser<'a> {
    properties: Vec<Property<'a>>,
    flags: Option<BTreeMap<String, (usize, bool)>>,
    base: Map<String, Value>,
    overrides: Map<String, Value>,
    stdin: Option<&'a str>,
}

impl<'a> ArgParser<'a> {
    fn new(properties: Vec<Property<'a>>, stdin: Option<&'a str>) -> Self {
        Self {
            properties,
            flags: None,
            base: Map::new(),
            overrides: Map::new(),
            stdin,
        }
    }

    fn parse_flag(&mut self, name: &str, source: ValueSource<'_>) -> Result<(), UsageError> {
        if name == "json" {
            let mut source = source;
            self.base = json_base(&mut source, self.stdin)?;
            return Ok(());
        }
        let (index, negated) = self.resolve_flag(name)?;
        let property = &self.properties[index];
        let value = parse_flag_value(property, negated, source)?;
        store_flag_value(&mut self.overrides, property, value);
        Ok(())
    }

    fn resolve_flag(&mut self, name: &str) -> Result<(usize, bool), UsageError> {
        if self.flags.is_none() {
            self.flags = Some(flag_table(&self.properties)?);
        }
        let table = self.flags.as_ref().unwrap();
        let &(index, negated) = table.get(name).ok_or_else(|| unknown_flag(name, table))?;
        Ok((index, negated))
    }
    fn finish(mut self, schema: &Value) -> Result<Value, UsageError> {
        self.base.extend(self.overrides);
        check_required(schema, &self.base)?;
        for property in &self.properties {
            if let Some(value) = self.base.get(property.key) {
                validate(property.schema, value)
                    .map_err(|reason| UsageError::new(format!("--{}: {reason}", property.flag)))?;
            }
        }
        Ok(Value::Object(self.base))
    }
}

fn store_flag_value(overrides: &mut Map<String, Value>, property: &Property<'_>, value: Value) {
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
}

fn parse_flag_value(
    property: &Property<'_>,
    negated: bool,
    mut source: ValueSource<'_>,
) -> Result<Value, UsageError> {
    if property.kind == FlagKind::Scalar(Scalar::Boolean) {
        if source.inline.is_some() {
            return Err(UsageError::new(format!(
                "--{} is a switch; use --{} or --no-{} without a value",
                property.flag, property.flag, property.flag
            )));
        }
        Ok(Value::Bool(!negated))
    } else {
        let text = source.read_value(&property.flag)?;
        match property.kind {
            FlagKind::Scalar(kind) | FlagKind::Repeated(kind) => parse_scalar(text, kind),
            FlagKind::Json => {
                serde_json::from_str(text).map_err(|error| format!("expected JSON: {error}"))
            }
        }
        .map_err(|reason| UsageError::new(format!("--{}: {reason}", property.flag)))
    }
}

fn positional_error(value: &str) -> UsageError {
    UsageError::new(format!(
        "unexpected argument {value:?}; use named --flags or --json (no positional arguments)"
    ))
}

fn json_base(
    source: &mut ValueSource<'_>,
    stdin: Option<&str>,
) -> Result<Map<String, Value>, UsageError> {
    let value = source.read_value("json")?;
    let text = if value == "-" {
        stdin.ok_or_else(|| UsageError::new("--json - requires stdin containing a JSON object"))?
    } else {
        value
    };
    match serde_json::from_str(text) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(UsageError::new("--json requires a JSON object")),
        Err(error) => Err(UsageError::new(format!("invalid --json object: {error}"))),
    }
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
