//! Compact schema transformation with cycle-safe local-reference rewriting.
//!
//! A catalog tool schema must be small (the whole catalog is one NATS KV
//! registration) but semantically complete. [`SchemaCompactor`] walks a source
//! schema and:
//!
//! - drops documentation-only keys (`description`, `example`/`examples`,
//!   `title`, `xml`, `externalDocs`, `x-*` vendor extensions);
//! - keeps every constraining keyword: `type`, `format`, `enum`, `const`,
//!   `pattern`, `minimum`/`maximum`, `minLength`/`maxLength`, `minItems`/
//!   `maxItems`, `uniqueItems`, `required`, `additionalProperties`,
//!   `patternProperties`, `nullable`, `default`, `discriminator`,
//!   `readOnly`/`writeOnly`, ...;
//! - replaces each local `$ref` with `#/$defs/<name>` and collects only the
//!   reachable definitions into a per-operation `$defs` map — including
//!   references that carry sibling keywords (the siblings describe this use,
//!   so cycles through them stay in the `$ref` form instead of inlining
//!   until the depth guard trips);
//! - rejects external references (`http://…`, `other.yaml#/…`, `#`) and
//!   unresolvable pointers with the operation context attached;
//! - never alters schema semantics. An upstream schema that contradicts
//!   itself (a `required` name it never declares while forbidding additional
//!   properties) is reported as a diagnostic and kept verbatim; deciding how
//!   to handle such operations is a planning decision, not a silent repair.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{SpecError, SpecResult};
use crate::refs::resolve_pointer;

/// Schema keys dropped from compact catalogs. Everything not listed and not
/// recursed into structurally is kept verbatim.
const DROPPED_KEYS: &[&str] = &[
    "description",
    "example",
    "examples",
    "title",
    "xml",
    "externalDocs",
    "$schema",
    "deprecated",
];

/// Child positions that hold schemas and must be walked.
const NESTED_OBJECT_KEYS: &[&str] = &[
    "properties",
    "patternProperties",
    "definitions",
    "$defs",
    "dependentSchemas",
];

/// Child positions that hold a list of schemas.
const NESTED_LIST_KEYS: &[&str] = &["allOf", "anyOf", "oneOf"];

/// Guard against hostile/deeply nested schemas.
const MAX_SCHEMA_DEPTH: usize = 128;

/// How many leading `#/$defs/…` references to follow when deriving wire
/// semantics (property names, binary detection) from a schema.
const MAX_REF_CHAIN: usize = 16;

/// Compacts one operation's schemas into a single reachable-`$defs` map.
pub struct SchemaCompactor<'a> {
    root: &'a Value,
    context: String,
    defs: Map<String, Value>,
    pointer_names: HashMap<String, String>,
    occupied: HashSet<String>,
    depth: usize,
    schema_diagnostics: Vec<String>,
}

impl<'a> SchemaCompactor<'a> {
    pub fn new(root: &'a Value, context: impl Into<String>) -> Self {
        Self {
            root,
            context: context.into(),
            defs: Map::new(),
            pointer_names: HashMap::new(),
            occupied: HashSet::new(),
            depth: 0,
            schema_diagnostics: Vec::new(),
        }
    }

    /// Diagnostics emitted while compacting this operation's schemas.
    pub fn take_diagnostics(&mut self) -> Vec<String> {
        std::mem::take(&mut self.schema_diagnostics)
    }

    /// Compact a source schema. References are rewritten into `#/$defs/…` and
    /// their definitions collected for [`SchemaCompactor::finish`].
    pub fn compact(&mut self, schema: &Value) -> SpecResult<Value> {
        self.depth += 1;
        if self.depth > MAX_SCHEMA_DEPTH {
            return Err(SpecError::unsupported(
                self.context.clone(),
                format!("schema nesting exceeds {MAX_SCHEMA_DEPTH} levels"),
            ));
        }
        let compacted = self.compact_inner(schema)?;
        self.depth -= 1;
        Ok(compacted)
    }

    fn compact_inner(&mut self, schema: &Value) -> SpecResult<Value> {
        match schema {
            Value::Bool(_) => Ok(schema.clone()),
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                    self.compact_reference(object, reference)
                } else {
                    self.compact_object(object)
                }
            }
            Value::Null => Ok(Value::Null),
            _ => Err(SpecError::invalid(
                self.context.clone(),
                format!("schema must be an object or boolean, got {schema}"),
            )),
        }
    }

    /// Compact `{ "type": "object", … }`, dispatching each key's shape.
    ///
    /// Cycle safety and reference handling live in [`Self::compact_reference`];
    /// this only walks structural positions and copies everything else
    /// verbatim.
    fn compact_object(&mut self, object: &Map<String, Value>) -> SpecResult<Value> {
        let mut result = Map::new();
        for (key, value) in object {
            if DROPPED_KEYS.contains(&key.as_str()) || key.starts_with("x-") {
                continue;
            }
            self.compact_key(&mut result, key, value)?;
        }
        self.record_contradiction_diagnostic(&result);
        Ok(Value::Object(result))
    }

    /// Compact one schema key at its correct structural position.
    fn compact_key(
        &mut self,
        result: &mut Map<String, Value>,
        key: &str,
        value: &Value,
    ) -> SpecResult<()> {
        match key {
            "items" => {
                // JSON Schema allows a single schema (Swagger 2.0/draft-4) or
                // a tuple of schemas; preserve the source shape.
                let compacted = match value {
                    Value::Array(items) => Value::Array(
                        items
                            .iter()
                            .map(|item| self.compact(item))
                            .collect::<SpecResult<Vec<_>>>()?,
                    ),
                    schema => self.compact(schema)?,
                };
                result.insert(key.to_string(), compacted);
            }
            "additionalProperties"
            | "additionalItems"
            | "not"
            | "if"
            | "then"
            | "else"
            | "contains"
            | "propertyNames" => {
                let compacted = if value.is_object() {
                    self.compact(value)?
                } else {
                    value.clone()
                };
                result.insert(key.to_string(), compacted);
            }
            key if NESTED_OBJECT_KEYS.contains(&key) => {
                if let Value::Object(entries) = value {
                    result.insert(key.to_string(), self.compact_entries(entries)?);
                }
            }
            key if NESTED_LIST_KEYS.contains(&key) => {
                if let Value::Array(entries) = value {
                    let compacted = entries
                        .iter()
                        .map(|entry| self.compact(entry))
                        .collect::<SpecResult<Vec<_>>>()?;
                    result.insert(key.to_string(), Value::Array(compacted));
                }
            }
            _ => {
                result.insert(key.to_string(), value.clone());
            }
        }
        Ok(())
    }

    /// Compact a `{name: schema, …}` map (`properties`, `$defs`, …).
    fn compact_entries(&mut self, entries: &Map<String, Value>) -> SpecResult<Value> {
        let mut compacted = Map::new();
        for (name, entry) in entries {
            compacted.insert(name.clone(), self.compact(entry)?);
        }
        Ok(Value::Object(compacted))
    }

    /// Compact `{"$ref": "…"}` with optional sibling keywords, which win over
    /// the target per JSON Schema reference semantics.
    ///
    /// The target is always collected into `$defs` under a stable name, like a
    /// bare reference. When the reference recurses while its target is still
    /// being compacted, the cycle is represented by the `$ref` form with the
    /// siblings merged in — never by inlining (which would recurse until the
    /// depth guard trips).
    fn compact_reference(
        &mut self,
        object: &Map<String, Value>,
        reference: &str,
    ) -> SpecResult<Value> {
        if object.len() == 1 {
            return self.reference_to_def(reference);
        }
        let name = self.def_name_for(reference)?;
        // `pointer_names` knows the reference before its target finishes
        // compacting; a missing `$defs` entry therefore means re-entry during
        // that compaction, i.e. a cycle.
        let expanded = match self.defs.get(&name) {
            Some(collected) => collected.clone(),
            None => json!({"$ref": format!("#/$defs/{name}")}),
        };
        let mut siblings = object.clone();
        siblings.remove("$ref");
        let merged = self.compact(&Value::Object(siblings))?;
        Ok(merge_objects(expanded, merged))
    }

    /// Rewrite a bare local `$ref` to `#/$defs/<name>`, compacting its target
    /// into the per-operation definitions map exactly once.
    fn reference_to_def(&mut self, reference: &str) -> SpecResult<Value> {
        let name = self.def_name_for(reference)?;
        Ok(json!({"$ref": format!("#/$defs/{name}")}))
    }

    /// The stable `$defs` name for a reference, collecting its target the
    /// first time it is seen.
    fn def_name_for(&mut self, reference: &str) -> SpecResult<String> {
        if let Some(name) = self.pointer_names.get(reference) {
            return Ok(name.clone());
        }
        let target = resolve_pointer(self.root, reference, &self.context)?;
        let name = self.allocate_def_name(reference);
        // Claim the name before compacting so nested references to the same
        // pointer see the cycle and keep the `$ref` form.
        self.pointer_names
            .insert(reference.to_string(), name.clone());
        let compacted = self.compact(target)?;
        self.defs.insert(name.clone(), compacted);
        Ok(name)
    }

    fn allocate_def_name(&mut self, reference: &str) -> String {
        let base = reference
            .rsplit('/')
            .next()
            .map(sanitize_def_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "def".to_string());
        if self.occupied.insert(base.clone()) {
            base
        } else {
            // A different pointer already claimed this name; keep both stable
            // by pinning the full pointer in a hash suffix.
            format!("{base}_{}", short_hash(reference))
        }
    }

    /// Record — without mutating anything — that an upstream schema
    /// contradicts itself: it lists a name under `required`, declares no
    /// property for it and forbids additional properties, so no request can
    /// validate. Schemas using `patternProperties` are skipped: a required
    /// name may be satisfiable through a pattern this release cannot prove or
    /// disprove. The operation stays registered with the schema verbatim;
    /// resolving such vendor typos is a planning decision, not a silent
    /// repair.
    fn record_contradiction_diagnostic(&mut self, result: &Map<String, Value>) {
        if result.get("additionalProperties") != Some(&Value::Bool(false)) {
            return;
        }
        if result.contains_key("patternProperties") {
            return;
        }
        let Some(required) = result.get("required").and_then(Value::as_array) else {
            return;
        };
        let declared = result.get("properties").and_then(Value::as_object);
        let undeclared: Vec<String> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| !declared.is_some_and(|properties| properties.contains_key(*name)))
            .map(str::to_string)
            .collect();
        if undeclared.is_empty() {
            return;
        }
        let names: Vec<String> = undeclared.iter().map(|name| format!("{name:?}")).collect();
        let diagnostic = format!(
            "{}: schema requires {} but declares no property for it while \
             additionalProperties is false, so no request can satisfy the schema (an \
             upstream/vendor typo); the schema is kept verbatim and the operation \
             cannot construct a valid request pending a plan decision",
            self.context,
            names.join(", ")
        );
        if !self.schema_diagnostics.contains(&diagnostic) {
            self.schema_diagnostics.push(diagnostic);
        }
    }

    /// The reachable, compacted definitions for the operation. Attach the
    /// result under `$defs` at the root of the operation's input schema.
    pub fn finish(self) -> Map<String, Value> {
        self.defs
    }
}

/// Merge `overriding` keys into `base` (both must be JSON objects).
fn merge_objects(base: Value, overriding: Value) -> Value {
    let (Some(mut base), Some(overriding)) = (base.as_object().cloned(), overriding.as_object())
    else {
        return base;
    };
    for (key, value) in overriding {
        base.insert(key.clone(), value.clone());
    }
    Value::Object(base)
}

fn sanitize_def_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// First 8 hex characters of the SHA-256 of `input` — a short, stable,
/// collision-safe disambiguator.
pub fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Follow up to [`MAX_REF_CHAIN`] leading `#/$defs/…` references.
pub(crate) fn resolve_def_chain<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> &'a Value {
    let mut current = schema;
    for _ in 0..MAX_REF_CHAIN {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let Some(name) = reference.strip_prefix("#/$defs/") else {
            break;
        };
        match defs.get(name) {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

/// Property names of an object-shaped schema, resolving one or more leading
/// `#/$defs/…` references through the operation's collected definitions.
///
/// Returns `None` unless the schema (after reference resolution) is a JSON
/// object with named properties; that is the only shape from which wire field
/// names may soundly be derived.
pub fn object_property_names(schema: &Value, defs: &Map<String, Value>) -> Option<Vec<String>> {
    let current = resolve_def_chain(schema, defs);
    current
        .get("properties")
        .and_then(Value::as_object)
        .filter(|properties| !properties.is_empty())
        .map(|properties| properties.keys().cloned().collect())
}

/// Whether a media-entry source schema describes raw bytes and therefore must
/// not be offered as a JSON `value` payload. Binary declarations behind a
/// local `$ref` are detected through the operation's collected definitions.
pub fn is_binary_schema(schema: &Value, defs: &Map<String, Value>) -> bool {
    let current = resolve_def_chain(schema, defs);
    let format = current.get("format").and_then(Value::as_str);
    format.is_some_and(|format| format == "binary" || format == "byte")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root() -> Value {
        json!({
            "components": {
                "schemas": {
                    "Node": {
                        "type": "object",
                        "description": "dropped",
                        "properties": {
                            "name": {"type": "string", "minLength": 1},
                            "children": {"type": "array", "items": {"$ref": "#/components/schemas/Node"}}
                        },
                        "required": ["name"]
                    },
                    "Pair": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/Node"}}},
                    "Tree": {"$ref": "#/components/schemas/Node"}
                },
                "parameters": {
                    "Node": {"name": "node", "in": "query", "schema": {"type": "string"}}
                }
            }
        })
    }

    #[test]
    fn rewrites_refs_collects_defs_and_drops_prose() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "TEST /test");
        let compacted = compactor
            .compact(&json!({"$ref": "#/components/schemas/Node"}))
            .unwrap();
        let defs = compactor.finish();
        assert_eq!(compacted, json!({"$ref": "#/$defs/Node"}));
        let node = &defs["Node"];
        assert_eq!(node["type"], "object");
        assert!(node.get("description").is_none());
        assert_eq!(node["properties"]["name"]["minLength"], 1);
        assert_eq!(
            node["properties"]["children"]["items"],
            json!({"$ref": "#/$defs/Node"})
        );
    }

    #[test]
    fn sibling_keywords_override_the_target() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "TEST /test");
        let compacted = compactor
            .compact(&json!({"$ref": "#/components/schemas/Node", "description": "use text"}))
            .unwrap();
        let defs = compactor.finish();
        assert!(compacted.get("$ref").is_none());
        assert_eq!(compacted["type"], "object");
        // The sibling use collects the target definition too.
        assert!(defs.contains_key("Node"));
    }

    #[test]
    fn recursive_refs_with_siblings_stay_cycle_safe() {
        harnx_core::require_nextest();
        // A schema whose every use of the recursive definition carries
        // sibling keywords used to inline the cycle until the depth guard
        // tripped; the $ref form must represent the recursion instead.
        let root = json!({
            "components": {"schemas": {
                "Loop": {
                    "type": "object",
                    "properties": {
                        "next": {"$ref": "#/components/schemas/Loop", "nullable": true},
                        "label": {"type": "string"}
                    },
                    "required": ["label"]
                }
            }}
        });
        let mut compactor = SchemaCompactor::new(&root, "TEST /loop");
        let compacted = compactor
            .compact(&json!({"$ref": "#/components/schemas/Loop"}))
            .unwrap();
        assert_eq!(compacted, json!({"$ref": "#/$defs/Loop"}));
        let defs = compactor.finish();
        let next = &defs["Loop"]["properties"]["next"];
        assert_eq!(next["nullable"], json!(true));
        assert_eq!(next["$ref"], json!("#/$defs/Loop"));
    }

    #[test]
    fn name_collisions_get_hash_suffix() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "TEST /test");
        let schema = json!({
            "type": "object",
            "properties": {
                "node": {"$ref": "#/components/schemas/Node"},
                "param": {"$ref": "#/components/parameters/Node"}
            }
        });
        let compacted = compactor.compact(&schema).unwrap();
        let defs = compactor.finish();
        let node_ref = compacted["properties"]["node"]["$ref"].as_str().unwrap();
        let param_ref = compacted["properties"]["param"]["$ref"].as_str().unwrap();
        assert_eq!(node_ref, "#/$defs/Node");
        let expected = format!(
            "#/$defs/Node_{}",
            short_hash("#/components/parameters/Node")
        );
        assert_eq!(param_ref, expected);
        assert!(defs.contains_key("Node"));
        assert!(defs.contains_key(param_ref.strip_prefix("#/$defs/").unwrap()));
    }

    #[test]
    fn chain_refs_resolve_through_defs() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "TEST /test");
        let compacted = compactor
            .compact(&json!({"$ref": "#/components/schemas/Tree"}))
            .unwrap();
        let defs = compactor.finish();
        assert_eq!(compacted, json!({"$ref": "#/$defs/Tree"}));
        let names = object_property_names(&defs["Tree"], &defs).unwrap();
        assert_eq!(names, vec!["name".to_string(), "children".to_string()]);
    }

    #[test]
    fn external_refs_are_rejected_with_context() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "POST /upload");
        let error = compactor
            .compact(&json!({"$ref": "https://example.com/schema.json"}))
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("POST /upload"), "{message}");
        assert!(
            message.contains("https://example.com/schema.json"),
            "{message}"
        );
    }

    #[test]
    fn missing_pointers_are_unresolved_refs() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "GET /x");
        let error = compactor
            .compact(&json!({"$ref": "#/components/schemas/Missing"}))
            .unwrap_err();
        assert!(
            matches!(error, SpecError::UnresolvedRef { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn contradictory_required_names_are_diagnosed_not_repaired() {
        harnx_core::require_nextest();
        let root = json!({});
        let mut compactor = SchemaCompactor::new(&root, "POST /broken");
        let contradictory = json!({
            "type": "object",
            "required": ["ghost"],
            "properties": {"real": {"type": "string"}},
            "additionalProperties": false
        });
        let compacted = compactor.compact(&contradictory).unwrap();
        // Kept verbatim: no semantic weakening.
        assert_eq!(compacted["additionalProperties"], json!(false));
        assert_eq!(compacted["required"], json!(["ghost"]));
        let diagnostics = compactor.take_diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(
            diagnostics[0].contains("POST /broken"),
            "{}",
            diagnostics[0]
        );
        assert!(diagnostics[0].contains("ghost"), "{}", diagnostics[0]);
        assert!(compactor.take_diagnostics().is_empty());
    }

    #[test]
    fn pattern_properties_do_not_trigger_the_contradiction_diagnostic() {
        harnx_core::require_nextest();
        let root = json!({});
        let mut compactor = SchemaCompactor::new(&root, "POST /pattern");
        // `ghost` may be satisfied by the pattern; this schema is not
        // provably contradictory.
        let satisfiable = json!({
            "type": "object",
            "required": ["ghost"],
            "properties": {"real": {"type": "string"}},
            "patternProperties": {"^gh": true},
            "additionalProperties": false
        });
        compactor.compact(&satisfiable).unwrap();
        assert!(compactor.take_diagnostics().is_empty());
    }

    #[test]
    fn binary_schemas_are_detected_through_refs() {
        harnx_core::require_nextest();
        let direct = json!({"type": "string", "format": "binary"});
        assert!(is_binary_schema(&direct, &Map::new()));
        assert!(!is_binary_schema(&json!({"type": "string"}), &Map::new()));
        let defs = Map::from_iter([(
            "Bytes".to_string(),
            json!({"type": "string", "format": "binary"}),
        )]);
        assert!(is_binary_schema(&json!({"$ref": "#/$defs/Bytes"}), &defs));
    }
}
