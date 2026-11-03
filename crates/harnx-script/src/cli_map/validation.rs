use serde_json::{Number, Value};

use crate::schema::{is_complex, scalar, Scalar};

pub(super) fn parse_scalar(text: &str, kind: Scalar) -> Result<Value, String> {
    match kind {
        Scalar::String => Ok(Value::String(text.to_owned())),
        Scalar::Boolean => parse_boolean(text),
        Scalar::Number | Scalar::Integer => parse_number(text, kind),
    }
}

fn parse_boolean(text: &str) -> Result<Value, String> {
    match text {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ => Err("expected boolean true or false".into()),
    }
}

fn parse_number(text: &str, kind: Scalar) -> Result<Value, String> {
    let number: Number = serde_json::from_str(text).map_err(|_| {
        format!(
            "expected {} in JSON numeric syntax, got {text:?}",
            kind.label()
        )
    })?;
    if kind == Scalar::Integer && !is_integer_number(&number) {
        return Err(
            "expected a signed/unsigned 64-bit integer (no decimal point or exponent)".into(),
        );
    }
    Ok(Value::Number(number))
}

fn is_integer_number(number: &Number) -> bool {
    number.is_i64() || number.is_u64()
}

/// Validate direct types and enums, not an entire JSON Schema dialect. References
/// and unions deliberately pass through; admission/provider validation owns them.
pub(super) fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    if let Some(kind) = scalar(schema) {
        validate_scalar_type(kind, value)?;
    } else if !is_complex(schema) {
        validate_compound_type(schema, value)?;
    }
    validate_enum(schema, value)?;
    Ok(())
}

fn validate_scalar_type(kind: Scalar, value: &Value) -> Result<(), String> {
    let valid = match kind {
        Scalar::String => value.is_string(),
        Scalar::Number => value.is_number(),
        Scalar::Integer => is_json_integer(value),
        Scalar::Boolean => value.is_boolean(),
    };
    if !valid {
        return Err(format!("expected {}, got {value}", kind.label()));
    }
    Ok(())
}

fn is_json_integer(value: &Value) -> bool {
    value.is_i64() || value.is_u64()
}

fn validate_compound_type(schema: &Value, value: &Value) -> Result<(), String> {
    match schema.get("type").and_then(Value::as_str) {
        Some("object") if !value.is_object() => Err("expected a JSON object".into()),
        Some("array") => validate_array(schema, value),
        Some("null") if !value.is_null() => Err("expected null".into()),
        _ => Ok(()),
    }
}

fn validate_array(schema: &Value, value: &Value) -> Result<(), String> {
    let items = value.as_array().ok_or("expected a JSON array")?;
    let Some(item_schema) = schema.get("items") else {
        return Ok(());
    };
    for item in items {
        validate(item_schema, item)?;
    }
    Ok(())
}

fn validate_enum(schema: &Value, value: &Value) -> Result<(), String> {
    let Some(options) = schema.get("enum").and_then(Value::as_array) else {
        return Ok(());
    };
    if !options.iter().any(|option| json_equal(option, value)) {
        return Err(format!("expected one of {}", Value::Array(options.clone())));
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
    let Some((float, integer)) = extract_float_and_int(a, b) else {
        return false;
    };
    if float.fract() != 0.0 {
        return false;
    }
    float_matches_integer(float, integer)
}

fn extract_float_and_int<'a>(a: &'a Number, b: &'a Number) -> Option<(f64, &'a Number)> {
    if a.is_f64() && !b.is_f64() {
        Some((a.as_f64()?, b))
    } else if b.is_f64() && !a.is_f64() {
        Some((b.as_f64()?, a))
    } else {
        None
    }
}

fn float_matches_integer(float: f64, integer: &Number) -> bool {
    if let Some(value) = integer.as_i64() {
        float_matches_i64(float, value)
    } else {
        float_matches_u64(float, integer.as_u64())
    }
}

fn float_matches_i64(float: f64, expected: i64) -> bool {
    if float < i64::MIN as f64 || float >= -(i64::MIN as f64) {
        return false;
    }
    float as i64 == expected
}

fn float_matches_u64(float: f64, expected: Option<u64>) -> bool {
    if float < 0.0 || float >= u64::MAX as f64 {
        return false;
    }
    Some(float as u64) == expected
}
