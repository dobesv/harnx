//! Cumulative budget for materialized JSON values produced by `$ref` siblings.

use serde_json::{Map, Value};

use crate::error::{SpecError, SpecResult};

/// Per-document traversal bound. Counted over every materialized JSON value.
pub(crate) const DEFAULT_MATERIALIZATION_VALUE_BUDGET: usize = 10_000_000;
/// Per-document estimated cloned-data bound. Each value reserves 64 bytes plus
/// UTF-8 bytes in strings and object keys; this bounds large scalar payloads too.
pub(crate) const DEFAULT_MATERIALIZATION_BYTE_BUDGET: usize = 256 * 1024 * 1024;

const ESTIMATED_BYTES_PER_VALUE: usize = 64;
pub(super) const TARGET_COPY_COUNT: usize = 2;
pub(super) const SIBLING_COPY_COUNT: usize = 3;

#[derive(Clone, Copy, Default)]
pub(crate) struct MaterializationCost {
    pub(crate) values: usize,
    pub(crate) bytes: usize,
}

impl MaterializationCost {
    pub(super) fn add(&mut self, other: Self) {
        self.values = self.values.saturating_add(other.values);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }

    pub(super) fn multiply(self, count: usize) -> Self {
        Self {
            values: self.values.saturating_mul(count),
            bytes: self.bytes.saturating_mul(count),
        }
    }
}

/// Count values and estimate allocated clone data iteratively, avoiding call
/// stack growth on nested JSON trees.
pub(super) fn retained_value_cost(value: &Value) -> MaterializationCost {
    count_values(vec![value], MaterializationCost::default())
}

fn count_values(mut pending: Vec<&Value>, mut cost: MaterializationCost) -> MaterializationCost {
    while let Some(value) = pending.pop() {
        add_value_cost(value, &mut cost);
        append_children(value, &mut pending, &mut cost);
    }
    cost
}

fn add_value_cost(value: &Value, cost: &mut MaterializationCost) {
    cost.add(MaterializationCost {
        values: 1,
        bytes: ESTIMATED_BYTES_PER_VALUE.saturating_add(match value {
            Value::String(text) => text.len(),
            _ => 0,
        }),
    });
}

fn append_children<'a>(
    value: &'a Value,
    pending: &mut Vec<&'a Value>,
    cost: &mut MaterializationCost,
) {
    match value {
        Value::Array(values) => pending.extend(values),
        Value::Object(object) => {
            for (key, value) in object {
                cost.bytes = cost.bytes.saturating_add(key.len());
                pending.push(value);
            }
        }
        _ => {}
    }
}

/// Upper bound for values cloned from a `$ref` object and retained as
/// siblings. Top-level `$ref` and prose-only siblings are omitted.
pub(super) fn retained_object_cost(object: &Map<String, Value>) -> MaterializationCost {
    let mut cost = MaterializationCost {
        values: 1,
        bytes: ESTIMATED_BYTES_PER_VALUE,
    };
    let mut pending = Vec::new();
    for (key, value) in object {
        if key == "$ref" || is_dropped_key(key) {
            continue;
        }
        cost.bytes = cost.bytes.saturating_add(key.len());
        pending.push(value);
    }
    if pending.is_empty() {
        MaterializationCost::default()
    } else {
        count_values(pending, cost)
    }
}

pub(super) fn is_dropped_key(key: &str) -> bool {
    matches!(
        key,
        "description"
            | "example"
            | "examples"
            | "title"
            | "xml"
            | "externalDocs"
            | "$schema"
            | "deprecated"
    ) || key.starts_with("x-")
}

pub(crate) struct MaterializationBudget {
    value_limit: usize,
    byte_limit: usize,
    values_used: usize,
    bytes_used: usize,
}

impl MaterializationBudget {
    pub(crate) fn new(value_limit: usize, byte_limit: usize) -> Self {
        Self {
            value_limit,
            byte_limit,
            values_used: 0,
            bytes_used: 0,
        }
    }

    pub(crate) fn used(&self) -> MaterializationCost {
        MaterializationCost {
            values: self.values_used,
            bytes: self.bytes_used,
        }
    }

    pub(crate) fn remaining(&self) -> (usize, usize) {
        (
            self.value_limit - self.values_used,
            self.byte_limit - self.bytes_used,
        )
    }

    /// Charge before clones or sibling traversal. Failed charges leave usage
    /// unchanged and identify both operation and reference.
    pub(crate) fn charge(
        &mut self,
        amount: MaterializationCost,
        context: &str,
        reference: &str,
    ) -> SpecResult<()> {
        let next_values = self.values_used.saturating_add(amount.values);
        let next_bytes = self.bytes_used.saturating_add(amount.bytes);
        if next_values > self.value_limit || next_bytes > self.byte_limit {
            return Err(SpecError::unsupported(
                format!("{context} (reference {reference})"),
                format!(
                    "cumulative schema reference materialization budget exceeded: \
                     {} / {} estimated bytes and {} / {} JSON values used; \
                     next expansion needs {} estimated bytes and {} JSON values",
                    self.bytes_used,
                    self.byte_limit,
                    self.values_used,
                    self.value_limit,
                    amount.bytes,
                    amount.values
                ),
            ));
        }
        self.values_used = next_values;
        self.bytes_used = next_bytes;
        Ok(())
    }
}
