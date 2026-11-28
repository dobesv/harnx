use harnx_openapi_tools::compact::{short_hash, SchemaCompactor};
use serde_json::{json, Value};

#[test]
fn literal_definition_cannot_overwrite_a_generated_suffix_target() {
    harnx_core::require_nextest();
    let mut root = json!({
        "components": {
            "schemas": {
                "Node": {"type": "string", "minLength": 1}
            },
            "parameters": {
                "Node": {"type": "string", "minLength": 7}
            }
        }
    });
    let parameter_reference = "#/components/parameters/Node";
    let literal_name = format!("Node_{}", short_hash(parameter_reference));
    root["components"]["schemas"]
        .as_object_mut()
        .unwrap()
        .insert(
            literal_name.clone(),
            json!({"type": "string", "minLength": 3}),
        );

    let compacted = json!({
        "type": "object",
        "properties": {
            "base": {"$ref": "#/components/schemas/Node"},
            "literal": {"$ref": format!("#/components/schemas/{literal_name}")},
            "parameter": {"$ref": parameter_reference}
        }
    });
    let mut compactor = SchemaCompactor::new(&root, "POST /name-collision");
    let mut schema = compactor.compact(&compacted).unwrap();
    let defs = compactor.finish().unwrap();
    let parameter_name = schema["properties"]["parameter"]["$ref"]
        .as_str()
        .unwrap()
        .strip_prefix("#/$defs/")
        .unwrap()
        .to_string();
    assert_ne!(parameter_name, literal_name);
    assert!(parameter_name.starts_with(&format!("{literal_name}_")));
    assert_eq!(defs["Node"]["minLength"], json!(1));
    assert_eq!(defs[&literal_name]["minLength"], json!(3));
    assert_eq!(defs[&parameter_name]["minLength"], json!(7));
    schema["$defs"] = Value::Object(defs);

    let raw_validator = jsonschema::validator_for(&schema).unwrap();
    let valid = json!({"base": "x", "literal": "xxx", "parameter": "1234567"});
    assert!(raw_validator.is_valid(&valid));
    assert!(!raw_validator.is_valid(&json!({"literal": "x"})));
    assert!(!raw_validator.is_valid(&json!({"parameter": "short"})));

    let core_schema = harnx_core::json_schema::JsonSchema::from_tool_schema(schema)
        .unwrap()
        .into_value();
    let core_validator = jsonschema::validator_for(&core_schema).unwrap();
    assert!(core_validator.is_valid(&valid));
    assert!(!core_validator.is_valid(&json!({"literal": "x"})));
    assert!(!core_validator.is_valid(&json!({"parameter": "short"})));
}
