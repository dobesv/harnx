//! The fields of a tool's structured result, declared for script authors.
//!
//! A script that passes `-o json` gets the whole structured result as one
//! JSON line. The schema declared here is what `help <tool>` lists so the
//! author knows which fields exist.

use serde_json::Value;

use crate::ToolSpec;

/// `_meta` key holding a tool's result schema.
pub const OUTPUT_SCHEMA_META_KEY: &str = "harnx:output_schema";

/// A `{"type": kind, "description": text}` property for a declared schema.
pub fn output_field(kind: &str, description: &str) -> Value {
    serde_json::json!({ "type": kind, "description": description })
}

/// Like [`output_field`] for a value that is `null` when there is nothing to
/// report.
pub fn output_nullable_field(kind: &str, description: &str) -> Value {
    serde_json::json!({ "type": [kind, "null"], "description": description })
}

/// The part of a result schema `help` shows: each top-level property's type
/// and the first line of its description. `None` when the schema has no
/// object `properties` or none of them.
pub fn output_schema_summary(schema: &Value) -> Option<Value> {
    let properties = schema.get("properties")?.as_object()?;
    let kept: serde_json::Map<String, Value> = properties
        .iter()
        .take(SUMMARY_MAX_PROPERTIES)
        .map(|(name, property)| (name.clone(), summarize_property(property)))
        .collect();
    if kept.is_empty() {
        return None;
    }
    Some(serde_json::json!({ "type": "object", "properties": kept }))
}

const SUMMARY_MAX_PROPERTIES: usize = 100;
const SUMMARY_MAX_DESCRIPTION_CHARS: usize = 200;

fn summarize_property(property: &Value) -> Value {
    let mut out = serde_json::Map::new();
    let kind = declared_type(property);
    if let Some(kind) = &kind {
        out.insert("type".to_string(), kind.clone());
    }
    if kind.as_ref().and_then(Value::as_str) == Some("array") {
        if let Some(item) = property.get("items").and_then(declared_type) {
            out.insert("items".to_string(), serde_json::json!({ "type": item }));
        }
    }
    if let Some(line) = property
        .get("description")
        .and_then(Value::as_str)
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
    {
        let cut: String = line.chars().take(SUMMARY_MAX_DESCRIPTION_CHARS).collect();
        out.insert("description".to_string(), Value::String(cut));
    }
    Value::Object(out)
}

/// A property's own `type`, else the member types of an `anyOf` or `oneOf`
/// (how pydantic and schemars write an optional value), else `object` for a
/// `$ref`.
fn declared_type(property: &Value) -> Option<Value> {
    if let Some(kind) = property.get("type") {
        return Some(kind.clone());
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(members) = property.get(key).and_then(Value::as_array) {
            let mut kinds: Vec<Value> = Vec::new();
            for kind in members.iter().filter_map(|m| m.get("type")?.as_str()) {
                let kind = Value::String(kind.to_string());
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
            return (!kinds.is_empty()).then_some(Value::Array(kinds));
        }
    }
    property
        .get("$ref")
        .map(|_| Value::String("object".to_string()))
}

impl ToolSpec {
    /// Declare the JSON Schema of this tool's `structuredContent`.
    #[must_use]
    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.meta
            .get_or_insert_with(serde_json::Map::new)
            .insert(OUTPUT_SCHEMA_META_KEY.to_string(), schema);
        self
    }

    /// Records the summary of a tool's own result schema unless the spec
    /// already declares one.
    #[must_use]
    pub fn with_summarized_output_schema(self, schema: &Value) -> Self {
        if self.output_schema().is_some() {
            return self;
        }
        match output_schema_summary(schema) {
            Some(summary) => self.with_output_schema(summary),
            None => self,
        }
    }

    /// The schema declared with [`ToolSpec::with_output_schema`].
    pub fn output_schema(&self) -> Option<&Value> {
        self.meta.as_ref()?.get(OUTPUT_SCHEMA_META_KEY)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn spec() -> ToolSpec {
        ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "fs_read".to_string(),
            description: "Read".to_string(),
            input_schema: json!({ "type": "object" }),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: None,
            meta: None,
        }
    }

    #[test]
    fn output_schema_round_trips_through_meta() {
        let schema = json!({"type": "object", "properties": {"text": {"type": "string"}}});
        let spec = spec().with_output_schema(schema.clone());
        assert_eq!(spec.output_schema(), Some(&schema));
        let wire = serde_json::to_value(&spec).expect("serialize spec");
        assert_eq!(wire["meta"]["harnx:output_schema"], schema);
        let back: ToolSpec = serde_json::from_value(wire).expect("deserialize spec");
        assert_eq!(back.output_schema(), Some(&schema));
    }

    #[test]
    fn output_schema_is_none_when_absent() {
        assert!(spec().output_schema().is_none());
    }

    #[test]
    fn output_schema_preserves_other_meta_keys() {
        let spec = spec()
            .with_call_template("call")
            .with_output_schema(json!({}));
        let meta = spec.meta.as_ref().expect("meta");
        assert_eq!(meta["call_template"], json!("call"));
        assert!(meta.contains_key(OUTPUT_SCHEMA_META_KEY));
        assert_eq!(OUTPUT_SCHEMA_META_KEY, "harnx:output_schema");
    }

    #[test]
    fn summary_keeps_only_type_items_and_first_description_line() {
        let schema = json!({
            "type": "object",
            "$defs": {"big": {"type": "object"}},
            "required": ["id"],
            "properties": {
                "id": {"type": "integer", "description": "\n  the id  \nsecond line", "minimum": 1},
                "tags": {"type": "array", "items": {"type": "string", "enum": ["a"]}},
                "nested": {"type": "object", "properties": {"deep": {"type": "string"}}},
            }
        });
        assert_eq!(
            output_schema_summary(&schema),
            Some(json!({"type": "object", "properties": {
                "id": {"type": "integer", "description": "the id"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "nested": {"type": "object"},
            }}))
        );
    }

    #[test]
    fn summary_types_for_any_of_and_ref() {
        let schema = json!({"properties": {
            "maybe": {"anyOf": [{"type": "string"}, {"type": "null"}, {"type": "string"}]},
            "one": {"oneOf": [{"type": "integer"}, {"type": "null"}]},
            "linked": {"$ref": "#/$defs/x"},
            "bare": {},
            "list": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "null"}]}},
        }});
        let summary = output_schema_summary(&schema).expect("summary");
        let props = &summary["properties"];
        assert_eq!(props["maybe"], json!({"type": ["string", "null"]}));
        assert_eq!(props["one"], json!({"type": ["integer", "null"]}));
        assert_eq!(props["linked"], json!({"type": "object"}));
        assert_eq!(props["bare"], json!({}));
        assert_eq!(
            props["list"],
            json!({"type": "array", "items": {"type": ["string", "null"]}})
        );
    }

    #[test]
    fn summary_drops_properties_past_the_hundredth() {
        let mut props = serde_json::Map::new();
        for n in 0..101 {
            props.insert(format!("p{n:03}"), json!({"type": "string"}));
        }
        let summary = output_schema_summary(&json!({"properties": props})).expect("summary");
        let kept = summary["properties"].as_object().expect("properties");
        assert_eq!(kept.len(), 100);
        assert!(kept.contains_key("p099"));
        assert!(!kept.contains_key("p100"));
    }

    #[test]
    fn summary_cuts_long_description_on_a_char_boundary() {
        let long = "é".repeat(300);
        let schema = json!({"properties": {"a": {"type": "string", "description": long}}});
        let summary = output_schema_summary(&schema).expect("summary");
        let text = summary["properties"]["a"]["description"]
            .as_str()
            .expect("text");
        assert_eq!(text.chars().count(), 200);
    }

    #[test]
    fn summary_is_none_without_usable_properties() {
        for schema in [
            json!("text"),
            json!({"type": "object"}),
            json!({"properties": {}}),
            json!({"properties": [1]}),
        ] {
            assert_eq!(output_schema_summary(&schema), None, "{schema}");
            assert!(spec()
                .with_summarized_output_schema(&schema)
                .output_schema()
                .is_none());
        }
    }

    #[test]
    fn summarized_schema_keeps_a_declared_one_byte_for_byte() {
        let own = json!({"properties": {"own": {"type": "string", "x": 1}}});
        let spec = spec()
            .with_output_schema(own.clone())
            .with_summarized_output_schema(&json!({"properties": {"other": {"type": "string"}}}));
        assert_eq!(spec.output_schema(), Some(&own));
    }

    #[test]
    fn output_field_names_type_and_description() {
        assert_eq!(
            output_field("string", "text"),
            json!({"type": "string", "description": "text"})
        );
    }

    #[test]
    fn nullable_field_adds_null_to_the_type() {
        assert_eq!(
            output_nullable_field("integer", "n"),
            json!({"type": ["integer", "null"], "description": "n"})
        );
    }
}
