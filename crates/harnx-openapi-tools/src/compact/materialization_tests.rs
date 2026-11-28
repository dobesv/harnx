use super::*;
use serde_json::json;

#[test]
fn sibling_reference_expansion_stops_at_cumulative_budget() {
    harnx_core::require_nextest();
    let mut schemas = serde_json::Map::new();
    schemas.insert("Leaf".to_string(), json!({"type": "string"}));
    for level in 0..24 {
        let previous = if level == 0 {
            "Leaf".to_string()
        } else {
            format!("Level{}", level - 1)
        };
        schemas.insert(
            format!("Level{level}"),
            json!({
                "allOf": [
                    {"$ref": format!("#/components/schemas/{previous}"), "minLength": level + 1},
                    {"$ref": format!("#/components/schemas/{previous}"), "maxLength": 100 - level}
                ]
            }),
        );
    }
    let root = json!({"components": {"schemas": schemas}});
    let top = json!({"$ref": "#/components/schemas/Level23", "maxLength": 200});
    let mut compactor = SchemaCompactor::with_budget(&root, "POST /bounded", 256, 1024 * 1024);
    let error = compactor.compact(&top).unwrap_err().to_string();
    assert!(error.contains("POST /bounded"), "{error}");
    assert!(error.contains("#/components/schemas/Level"), "{error}");
    assert!(error.contains("budget exceeded"), "{error}");
    assert!(error.contains("256 JSON values"), "{error}");

    let reuse_error = compactor.compact(&json!({"type": "string"})).unwrap_err();
    assert!(reuse_error
        .to_string()
        .contains("cannot be reused after a failed compaction"));
    assert!(compactor.finish().is_err());
}

#[test]
fn discarded_prose_siblings_do_not_expand_collected_reference() {
    harnx_core::require_nextest();
    let root = json!({
        "components": {"schemas": {
            "Large": {"type": "object", "properties": {
                "first": {"type": "string", "minLength": 1},
                "second": {"type": "string", "maxLength": 12}
            }}
        }}
    });
    let mut compactor = SchemaCompactor::with_budget(&root, "POST /prose", 0, 0);
    let first = compactor
        .compact(&json!({"$ref": "#/components/schemas/Large"}))
        .unwrap();
    let with_prose = compactor
        .compact(&json!({
            "$ref": "#/components/schemas/Large",
            "description": "discarded",
            "x-note": "discarded"
        }))
        .unwrap();

    assert_eq!(first, with_prose);
    assert_eq!(with_prose["$ref"], json!("#/$defs/Large"));
    assert_eq!(compactor.materialization_used().values, 0);
    assert_eq!(compactor.materialization_used().bytes, 0);
    assert!(compactor.finish().unwrap().contains_key("Large"));
}
