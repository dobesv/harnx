//! Shared parsing of `--data KEY=VALUE` and `--data-json` input values
//! through the T4.2 interaction response contract.
//!
//! Used by `harnx interact answer` and the T5.1 `harnx surface act` CLI
//! fallback, so both commands type input values identically.

use anyhow::{anyhow, bail, Context, Result};
use harnx_core::interaction::response::{input_fields, InputKind, OptionItem};
use serde_json::{json, Map, Value};

/// Parse `--data-json` into input values keyed by input binding. Values are
/// used exactly as typed; durable acceptance type-checks them.
pub(crate) fn json_values(payload: &Value, raw: &str) -> Result<Map<String, Value>> {
    let value: Value = serde_json::from_str(raw).context("--data-json must be a JSON object")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("--data-json must be a JSON object, got {raw}"))?;
    let fields = input_fields(payload);
    for key in object.keys() {
        anyhow::ensure!(
            fields.iter().any(|field| &field.key == key),
            "unknown input key '{key}'"
        );
    }
    Ok(object.clone())
}

/// One `--data KEY=VALUE` pair, typed by the input it names. The key may be
/// the bare key or the full pointer (`/name=Ada`). A ChoicePicker value is a
/// comma-separated list of option values or labels, like the shared text
/// grammar; a TextField value keeps its spaces.
pub(crate) fn insert_data_value(
    payload: &Value,
    pair: &str,
    values: &mut Map<String, Value>,
) -> Result<()> {
    let (key, value) = pair
        .split_once('=')
        .with_context(|| format!("expected KEY=VALUE, got '{pair}'"))?;
    let key = key.strip_prefix('/').unwrap_or(key);
    let field = input_fields(payload)
        .into_iter()
        .find(|field| field.key == key)
        .ok_or_else(|| anyhow!("unknown input key '{key}'"))?;
    let value = match field.kind {
        InputKind::TextField => json!(value),
        InputKind::ChoicePicker => choice_value(&field.options, &field.key, value)?,
    };
    values.insert(field.key, value);
    Ok(())
}

/// Each comma-separated item must be a declared option value, or a label
/// that maps to exactly one value.
pub(crate) fn choice_value(options: &[OptionItem], key: &str, value: &str) -> Result<Value> {
    let mut selected = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if let Some(value) = options
            .iter()
            .find(|option| option.value == item)
            .map(|option| option.value.clone())
        {
            selected.push(value);
            continue;
        }
        let labeled: Vec<String> = options
            .iter()
            .filter(|option| option.label == item)
            .map(|option| option.value.clone())
            .collect();
        match labeled.as_slice() {
            [value] => selected.push(value.clone()),
            [] => bail!("'{item}' is neither an option value nor a label of input '{key}'"),
            _ => bail!(
                "label '{item}' matches {} options of input '{key}'; use the option value",
                labeled.len()
            ),
        }
    }
    Ok(json!(selected))
}
