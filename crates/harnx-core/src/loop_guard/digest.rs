//! Order-insensitive hashing of JSON values.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub(crate) type Fingerprint = [u8; 32];

/// Hash a JSON value so objects compare regardless of key order. The
/// workspace builds serde_json with `preserve_order`, and models emit the same
/// arguments with their keys in a different order from call to call.
pub(crate) fn fingerprint(value: &Value) -> Fingerprint {
    let bytes = serde_json::to_vec(&sorted(value)).expect("a JSON value always serializes");
    Sha256::digest(&bytes).into()
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::with_capacity(map.len());
            for key in keys {
                out.insert(key.clone(), sorted(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_does_not_change_the_fingerprint() {
        // Gemini sent these three keys in all six orders across one loop.
        let orders = [
            r#"{"limit":70,"offset":70,"path":"a.rs"}"#,
            r#"{"limit":70,"path":"a.rs","offset":70}"#,
            r#"{"offset":70,"limit":70,"path":"a.rs"}"#,
            r#"{"offset":70,"path":"a.rs","limit":70}"#,
            r#"{"path":"a.rs","limit":70,"offset":70}"#,
            r#"{"path":"a.rs","offset":70,"limit":70}"#,
        ];
        let first = fingerprint(&serde_json::from_str(orders[0]).unwrap());
        for order in orders {
            assert_eq!(
                fingerprint(&serde_json::from_str(order).unwrap()),
                first,
                "{order}"
            );
        }
    }

    #[test]
    fn nested_objects_are_order_insensitive_too() {
        let a = json!({"outer": {"b": 1, "a": [{"y": 2, "x": 1}]}});
        let b: Value = serde_json::from_str(r#"{"outer":{"a":[{"x":1,"y":2}],"b":1}}"#).unwrap();
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn different_values_differ() {
        assert_ne!(
            fingerprint(&json!({"offset": 70})),
            fingerprint(&json!({"offset": 71}))
        );
        assert_ne!(fingerprint(&json!([1, 2])), fingerprint(&json!([2, 1])));
    }
}
