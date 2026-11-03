use std::collections::BTreeMap;

use serde_json::Value;

use crate::schema::{flag_table, properties, FlagKind, Property, Scalar};

use super::UsageError;

/// Output rendering belongs to the tool-command adapter, not tool arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Json,
    Text,
}

/// Extract `-o json|text` (or `-o=json|text`), leaving tool arguments intact.
/// Repeated `-o` uses the last value. `--` stops extraction. Schema-aware value
/// handling keeps `--path -o` literal, while `--boolean -o json` selects JSON.
/// `--output` isn't reserved; a tool property named `o` uses the long flag `--o`.
pub fn split_output_args(
    schema: &Value,
    argv: &[String],
) -> Result<(Vec<String>, Option<OutputFormat>), UsageError> {
    let properties = properties(schema);
    // Keep errors deferred so --json remains usable for conflicting schemas.
    let flags = flag_table(&properties);
    let mut args = Vec::new();
    let mut format = None;
    let mut index = 0;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            args.extend_from_slice(&argv[index..]);
            break;
        }
        if is_output_flag(arg) {
            let value = extract_output_value(arg, argv, &mut index)?;
            format = Some(parse_format(value)?);
        } else {
            args.push(arg.clone());
            if should_consume_next_arg(arg, &flags, &properties, argv.get(index + 1))? {
                index += 1;
                args.push(argv[index].clone());
            }
        }
        index += 1;
    }
    Ok((args, format))
}

fn is_output_flag(arg: &str) -> bool {
    arg == "-o" || arg.starts_with("-o=")
}

fn extract_output_value<'a>(
    arg: &'a str,
    argv: &'a [String],
    index: &mut usize,
) -> Result<&'a str, UsageError> {
    if let Some((_, value)) = arg.split_once('=') {
        Ok(value)
    } else {
        *index += 1;
        argv.get(*index)
            .map(String::as_str)
            .ok_or_else(|| UsageError::new("-o requires json or text"))
    }
}

fn should_consume_next_arg(
    arg: &str,
    flags: &Result<BTreeMap<String, (usize, bool)>, UsageError>,
    properties: &[Property<'_>],
    next_arg: Option<&String>,
) -> Result<bool, UsageError> {
    let Some(name) = arg.strip_prefix("--") else {
        return Ok(false);
    };
    if name.contains('=') {
        return Ok(false);
    }
    if !flag_takes_value(name, flags, properties)? {
        return Ok(false);
    }
    Ok(next_arg.is_some_and(|value| !value.starts_with("--")))
}

fn flag_takes_value(
    name: &str,
    flags: &Result<BTreeMap<String, (usize, bool)>, UsageError>,
    properties: &[Property<'_>],
) -> Result<bool, UsageError> {
    if name == "json" {
        return Ok(true);
    }
    let flag_map = flags.as_ref().map_err(Clone::clone)?;
    let takes_val = flag_map
        .get(name)
        .is_some_and(|(index, _)| properties[*index].kind != FlagKind::Scalar(Scalar::Boolean));
    Ok(takes_val)
}

fn parse_format(value: &str) -> Result<OutputFormat, UsageError> {
    match value {
        "json" => Ok(OutputFormat::Json),
        "text" => Ok(OutputFormat::Text),
        _ => Err(UsageError::new(format!(
            "invalid -o value {value:?}; expected json or text"
        ))),
    }
}
