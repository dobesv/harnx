//! Monty's serde format is tagged Rust data, not JSON-shaped Python values.

use anyhow::{anyhow, bail, ensure, Result};
use harnx_script::{MontyObject, ObjectRef};
use serde_json::{Number, Value};

const MAX_JSON_DEPTH: usize = 64;

pub(crate) fn to_python(value: Value) -> Result<MontyObject> {
    json_to_python(value, 0)
}

fn json_to_python(value: Value, depth: usize) -> Result<MontyObject> {
    ensure!(
        depth < MAX_JSON_DEPTH,
        "middleware JSON nesting exceeds {MAX_JSON_DEPTH} levels"
    );
    match value {
        Value::Null => Ok(MontyObject::none()),
        Value::Bool(value) => Ok(MontyObject::bool(value)),
        Value::String(value) => Ok(MontyObject::string(value)),
        Value::Number(value) => number_to_python(value),
        Value::Array(values) => values
            .into_iter()
            .map(|value| json_to_python(value, depth + 1))
            .collect::<Result<Vec<_>>>()
            .map(MontyObject::list),
        Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| Ok((MontyObject::string(key), json_to_python(value, depth + 1)?)))
            .collect::<Result<Vec<_>>>()
            .map(MontyObject::dict),
    }
}

fn number_to_python(number: Number) -> Result<MontyObject> {
    if let Some(value) = number.as_i64() {
        Ok(MontyObject::int(value))
    } else if let Some(value) = number.as_u64() {
        Ok(MontyObject::bigint(value.into()))
    } else {
        number
            .as_f64()
            .map(MontyObject::float)
            .ok_or_else(|| anyhow!("JSON number cannot be represented in Python"))
    }
}

pub(crate) fn from_python(value: MontyObject) -> Result<Value> {
    convert(value.as_ref(), 0)
}

fn convert(value: ObjectRef<'_>, depth: usize) -> Result<Value> {
    ensure!(
        depth < MAX_JSON_DEPTH,
        "Python middleware JSON nesting exceeds {MAX_JSON_DEPTH} levels"
    );
    // `type_name` is exact for the JSON-shaped types: a set, namedtuple or
    // class instance reports its own name and falls through to the error.
    match value.type_name() {
        "NoneType" => Ok(Value::Null),
        "bool" => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| anyhow!("Python bool has no boolean value")),
        // The decimal form covers every size, including past `i64`.
        "int" => integer_to_json(&value.py_repr()),
        "float" => value
            .as_float()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| anyhow!("non-finite Python number is not JSON")),
        "str" => value
            .as_str()
            .map(|text| Value::String(text.to_owned()))
            .ok_or_else(|| anyhow!("Python str has no text value")),
        "list" | "tuple" => value
            .items()
            .ok_or_else(|| anyhow!("Python sequence has no items"))?
            .into_iter()
            .map(|item| convert(item, depth + 1))
            .collect::<Result<_>>()
            .map(Value::Array),
        "dict" => value
            .pairs()
            .ok_or_else(|| anyhow!("Python dict has no entries"))?
            .into_iter()
            .map(|pair| dict_entry(pair, depth))
            .collect::<Result<_>>()
            .map(Value::Object),
        _ => bail!("Python middleware value is not JSON-compatible"),
    }
}

fn dict_entry(
    (key, value): (ObjectRef<'_>, ObjectRef<'_>),
    depth: usize,
) -> Result<(String, Value)> {
    let Some(key) = key.as_str() else {
        bail!("Python JSON object keys must be strings");
    };
    Ok((key.to_owned(), convert(value, depth + 1)?))
}

// serde_json parses larger integers as f64; reject them rather than round tool arguments.
fn integer_to_json(value: &str) -> Result<Value> {
    value
        .parse::<i64>()
        .map(Number::from)
        .or_else(|_| value.parse::<u64>().map(Number::from))
        .map(Value::Number)
        .map_err(|_| anyhow!("Python integer exceeds JSON's supported integer range"))
}

#[cfg(test)]
mod tests {
    use super::{from_python, to_python, MAX_JSON_DEPTH};
    use harnx_script::MontyObject;
    use serde_json::{json, Value};

    #[test]
    fn middleware_python_json_round_trip_preserves_types_and_integer_precision() {
        let json =
            json!({"null":null,"bool":true,"text":"hello", "values":[-1,1.25, i64::MIN, u64::MAX]});
        assert_eq!(from_python(to_python(json.clone()).unwrap()).unwrap(), json);
    }

    #[test]
    fn middleware_python_rejects_unrepresentable_numbers_and_object_keys() {
        for value in [
            MontyObject::float(f64::INFINITY),
            MontyObject::float(f64::NAN),
            MontyObject::bigint(u128::MAX.into()),
            MontyObject::dict(vec![(MontyObject::int(1), MontyObject::none())]),
        ] {
            assert!(from_python(value).is_err());
        }
    }

    #[test]
    fn middleware_python_json_conversion_bounds_nesting_in_both_directions() {
        let mut json = Value::Null;
        let mut python = MontyObject::none();
        for _ in 0..MAX_JSON_DEPTH {
            json = Value::Array(vec![json]);
            python = MontyObject::list(vec![python]);
        }
        assert!(to_python(json).is_err());
        assert!(from_python(python).is_err());
    }
}
