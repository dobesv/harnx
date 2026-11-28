//! Stateful schema traversal and reference compaction.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::error::{SpecError, SpecResult};
use crate::refs::resolve_pointer;

use super::definition_names::allocate_def_name;
use super::diagnostics::record_contradiction_diagnostic;
use super::materialization::{
    is_dropped_key, retained_object_cost, retained_value_cost, MaterializationBudget,
    MaterializationCost, DEFAULT_MATERIALIZATION_BYTE_BUDGET, DEFAULT_MATERIALIZATION_VALUE_BUDGET,
    SIBLING_COPY_COUNT, TARGET_COPY_COUNT,
};

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

/// Compacts one operation's schemas into a single reachable-`$defs` map.
pub struct SchemaCompactor<'a> {
    root: &'a Value,
    context: String,
    defs: Map<String, Value>,
    definition_costs: HashMap<String, MaterializationCost>,
    pointer_names: HashMap<String, String>,
    occupied: HashSet<String>,
    depth: usize,
    schema_diagnostics: Vec<String>,
    materialization_budget: MaterializationBudget,
    failed: bool,
}

impl<'a> SchemaCompactor<'a> {
    pub fn new(root: &'a Value, context: impl Into<String>) -> Self {
        Self {
            root,
            context: context.into(),
            defs: Map::new(),
            definition_costs: HashMap::new(),
            pointer_names: HashMap::new(),
            occupied: HashSet::new(),
            depth: 0,
            schema_diagnostics: Vec::new(),
            materialization_budget: MaterializationBudget::new(
                DEFAULT_MATERIALIZATION_VALUE_BUDGET,
                DEFAULT_MATERIALIZATION_BYTE_BUDGET,
            ),
            failed: false,
        }
    }

    pub(crate) fn with_budget(
        root: &'a Value,
        context: impl Into<String>,
        value_budget: usize,
        byte_budget: usize,
    ) -> Self {
        Self {
            root,
            context: context.into(),
            defs: Map::new(),
            definition_costs: HashMap::new(),
            pointer_names: HashMap::new(),
            occupied: HashSet::new(),
            depth: 0,
            schema_diagnostics: Vec::new(),
            materialization_budget: MaterializationBudget::new(value_budget, byte_budget),
            failed: false,
        }
    }

    pub(crate) fn materialization_used(&self) -> MaterializationCost {
        self.materialization_budget.used()
    }

    /// Diagnostics emitted while compacting this operation's schemas.
    pub fn take_diagnostics(&mut self) -> Vec<String> {
        std::mem::take(&mut self.schema_diagnostics)
    }

    /// Compact a source schema. References are rewritten into `#/$defs/…` and
    /// their definitions collected for [`SchemaCompactor::finish`].
    pub fn compact(&mut self, schema: &Value) -> SpecResult<Value> {
        if self.failed {
            return Err(SpecError::unsupported(
                self.context.clone(),
                "schema compactor cannot be reused after a failed compaction".to_string(),
            ));
        }
        self.depth += 1;
        if self.depth > MAX_SCHEMA_DEPTH {
            self.depth -= 1;
            self.failed = true;
            return Err(SpecError::unsupported(
                self.context.clone(),
                format!("schema nesting exceeds {MAX_SCHEMA_DEPTH} levels"),
            ));
        }
        let result = self.compact_inner(schema);
        self.depth -= 1;
        if result.is_err() {
            self.failed = true;
        }
        result
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
            if is_dropped_key(key) {
                continue;
            }
            self.compact_key(&mut result, key, value)?;
        }
        record_contradiction_diagnostic(&result, &self.context, &mut self.schema_diagnostics);
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
        if has_no_retained_siblings(object) {
            self.reference_to_def(reference)
        } else {
            self.compact_sibling_reference(object, reference)
        }
    }

    fn compact_sibling_reference(
        &mut self,
        object: &Map<String, Value>,
        reference: &str,
    ) -> SpecResult<Value> {
        let name = self.def_name_for(reference)?;
        self.compact_reference_with_siblings(object, reference, &name)
    }

    fn compact_reference_with_siblings(
        &mut self,
        object: &Map<String, Value>,
        reference: &str,
        name: &str,
    ) -> SpecResult<Value> {
        self.charge_reference_expansion(object, reference, name)?;
        let expanded = self.expanded_reference(name);
        let compacted_siblings = self.compact_retained_siblings(object)?;
        Ok(merge_objects(expanded, compacted_siblings))
    }

    fn charge_reference_expansion(
        &mut self,
        object: &Map<String, Value>,
        reference: &str,
        name: &str,
    ) -> SpecResult<()> {
        // Charge before cloning the target or traversing retained siblings.
        let sibling_cost = retained_object_cost(object).multiply(SIBLING_COPY_COUNT);
        let target_cost = self
            .definition_costs
            .get(name)
            .copied()
            .unwrap_or_default()
            .multiply(TARGET_COPY_COUNT);
        let mut cost = target_cost;
        cost.add(sibling_cost);
        self.materialization_budget
            .charge(cost, &self.context, reference)
    }

    fn expanded_reference(&self, name: &str) -> Value {
        // A missing definition means re-entry while the target is being
        // compacted, so preserve the cycle as a reference.
        match self.defs.get(name) {
            Some(collected) => collected.clone(),
            None => json!({"$ref": format!("#/$defs/{name}")}),
        }
    }

    fn compact_retained_siblings(&mut self, object: &Map<String, Value>) -> SpecResult<Value> {
        let siblings = object
            .iter()
            .filter(|(key, _)| key.as_str() != "$ref" && !is_dropped_key(key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        self.compact(&Value::Object(siblings))
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
        let name = allocate_def_name(reference, &mut self.occupied, &self.context)?;
        // Claim the name before compacting so nested references to the same
        // pointer see the cycle and keep the `$ref` form.
        self.pointer_names
            .insert(reference.to_string(), name.clone());
        let compacted = self.compact(target)?;
        self.definition_costs
            .insert(name.clone(), retained_value_cost(&compacted));
        self.defs.insert(name.clone(), compacted);
        Ok(name)
    }

    /// The reachable, compacted definitions for the operation. Attach the
    /// result under `$defs` at the root of the operation's input schema.
    /// Return collected definitions. A compactor whose `compact` call failed
    /// is poisoned; its partial definitions must not be published or reused.
    pub fn finish(self) -> SpecResult<Map<String, Value>> {
        if self.failed {
            return Err(SpecError::unsupported(
                self.context,
                "schema compactor cannot be finished after a failed compaction".to_string(),
            ));
        }
        Ok(self.defs)
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

fn has_no_retained_siblings(object: &Map<String, Value>) -> bool {
    object
        .keys()
        .all(|key| key == "$ref" || is_dropped_key(key))
}
