use serde_json::{Number, Value};

use crate::schema::{is_complex, scalar, Scalar};

pub(super) fn parse_scalar(text: &str, kind: Scalar) -> Result<Value, String> {
    match kind {
        Scalar::String => Ok(Value::String(text.to_owned())),
        Scalar::Boolean => match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err("expected boolean true or false".into()),
        },
        Scalar::Number | Scalar::Integer => {
            let number: Number = serde_json::from_str(text).map_err(|_| {
                format!(
                    "expected {} in JSON numeric syntax, got {text:?}",
                    kind.label()
                )
            })?;
            if kind == Scalar::Integer && !number.is_i64() && !number.is_u64() {
                return Err(
                    "expected a signed/unsigned 64-bit integer (no decimal point or exponent)"
                        .into(),
                );
            }
            Ok(Value::Number(number))
        }
    }
}

/// Validate direct types and enums, not an entire JSON Schema dialect. References
/// and unions deliberately pass through; admission/provider validation owns them.
pub(super) fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    if let Some(kind) = scalar(schema) {
        let valid = match kind {
            Scalar::String => value.is_string(),
            Scalar::Number => value.is_number(),
            Scalar::Integer => value.is_i64() || value.is_u64(),
            Scalar::Boolean => value.is_boolean(),
        };
        if !valid {
            return Err(format!("expected {}, got {value}", kind.label()));
        }
    } else if !is_complex(schema) {
        match schema.get("type").and_then(Value::as_str) {
            Some("object") if !value.is_object() => return Err("expected a JSON object".into()),
            Some("array") => {
                let items = value.as_array().ok_or("expected a JSON array")?;
                if let Some(item_schema) = schema.get("items") {
                    for item in items {
                        validate(item_schema, item)?;
                    }
                }
            }
            Some("null") if !value.is_null() => return Err("expected null".into()),
            _ => {}
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.iter().any(|option| json_equal(option, value)) {
            return Err(format!("expected one of {}", Value::Array(options.clone())));
        }
    }
    Ok(())
}

fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => number_equal(a, b),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| json_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| json_equal(a, b)))
        }
        _ => a == b,
    }
}

// JSON Schema numeric equality treats 1 and 1.0 alike. Don't compare all integers
// through f64: adjacent large integers can round to the same floating value.
fn number_equal(a: &Number, b: &Number) -> bool {
    if a == b {
        return true;
    }
    let (float, integer) = if a.is_f64() && !b.is_f64() {
        (a.as_f64().unwrap(), b)
    } else if b.is_f64() && !a.is_f64() {
        (b.as_f64().unwrap(), a)
    } else {
        return false;
    };
    if float.fract() != 0.0 {
        return false;
    }
    if let Some(integer) = integer.as_i64() {
        float >= i64::MIN as f64 && float < -(i64::MIN as f64) && float as i64 == integer
    } else {
        float >= 0.0 && float < u64::MAX as f64 && Some(float as u64) == integer.as_u64()
    }
}
