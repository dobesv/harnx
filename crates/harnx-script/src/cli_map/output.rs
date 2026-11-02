use serde_json::Value;

use crate::schema::{flag_table, properties, FlagKind, Scalar};

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
        if arg == "-o" || arg.starts_with("-o=") {
            let value = if let Some((_, value)) = arg.split_once('=') {
                value
            } else {
                index += 1;
                argv.get(index)
                    .ok_or_else(|| UsageError::new("-o requires json or text"))?
            };
            format = Some(parse_format(value)?);
        } else {
            args.push(arg.clone());
            if let Some(name) = arg.strip_prefix("--").filter(|name| !name.contains('=')) {
                let takes_value = if name == "json" {
                    true
                } else {
                    flags
                        .as_ref()
                        .map_err(Clone::clone)?
                        .get(name)
                        .is_some_and(|(index, _)| {
                            properties[*index].kind != FlagKind::Scalar(Scalar::Boolean)
                        })
                };
                if takes_value
                    && argv
                        .get(index + 1)
                        .is_some_and(|value| !value.starts_with("--"))
                {
                    index += 1;
                    args.push(argv[index].clone());
                }
            }
        }
        index += 1;
    }
    Ok((args, format))
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
