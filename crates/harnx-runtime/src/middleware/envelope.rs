//! Script-visible envelopes never serialize host-owned result snapshots.

use anyhow::{anyhow, Result};
use harnx_core::tool::{ToolCall, ToolResult};
use harnx_engine::tool::{InnerToolCallStatus, MaterializedToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    output: Value,
    is_error: bool,
    #[serde(rename = "ref")]
    reference: Option<String>,
}

#[derive(Default)]
pub(super) struct Snapshots {
    results: HashMap<String, MaterializedToolResult>,
}

pub(super) fn is_error(output: &Value) -> bool {
    output["is_error"] == true || output["isError"] == true
}

impl Snapshots {
    pub fn insert(&mut self, result: MaterializedToolResult) -> Value {
        let reference = uuid::Uuid::new_v4().to_string();
        let envelope = json!({
            "output": result.result.output,
            "is_error": is_error(&result.result.output) || result.status != InnerToolCallStatus::Ok,
            "ref": reference,
        });
        self.results.insert(reference, result);
        envelope
    }

    pub fn restore(&mut self, reference: String, snapshot: MaterializedToolResult) {
        self.results.insert(reference, snapshot);
    }

    pub fn retained<'a>(
        &self,
        references: impl Iterator<Item = &'a String>,
    ) -> HashMap<String, MaterializedToolResult> {
        references
            .filter_map(|reference| {
                self.results
                    .get(reference)
                    .map(|snapshot| (reference.clone(), snapshot.clone()))
            })
            .collect()
    }

    // qual:allow(iosp) reason: "Iterative JSON traversal validates nested envelopes without recursive stack growth."
    pub fn references(&self, value: &Value) -> Result<Vec<String>> {
        let mut references = Vec::new();
        let mut pending = vec![value];
        while let Some(value) = pending.pop() {
            references.extend(self.envelope_reference(value)?);
            pending.extend(children(value));
        }
        Ok(references)
    }

    fn envelope_reference(&self, value: &Value) -> Result<Option<String>> {
        if value.get("output").is_none()
            || value.get("is_error").is_none()
            || value.get("ref").is_none()
        {
            return Ok(None);
        }
        self.validate(value)?;
        Ok(value["ref"].as_str().map(str::to_owned))
    }

    pub fn resolve(&self, call: &ToolCall, value: Value) -> Result<MaterializedToolResult> {
        let envelope: Envelope = serde_json::from_value(value)?;
        let mut result = match envelope.reference {
            Some(reference) => self
                .results
                .get(&reference)
                .cloned()
                .ok_or_else(|| anyhow!("unknown or wrong-scope middleware result ref"))?,
            None => MaterializedToolResult::new(
                ToolResult::new(call.clone(), Value::Null),
                InnerToolCallStatus::Ok,
            ),
        };
        result.result.output = normalize(envelope.output, envelope.is_error);
        result.result.markdown = None;
        result.status = if is_error(&result.result.output) {
            InnerToolCallStatus::ToolError
        } else {
            InnerToolCallStatus::Ok
        };
        if result.status != InnerToolCallStatus::Ok {
            result.result.switch_agent = None;
        }
        Ok(result)
    }

    pub fn validate(&self, value: &Value) -> Result<()> {
        let envelope: Envelope = serde_json::from_value(value.clone())?;
        if let Some(reference) = envelope.reference {
            self.results
                .get(&reference)
                .ok_or_else(|| anyhow!("unknown or wrong-scope middleware result ref"))?;
        }
        Ok(())
    }
}

fn normalize(output: Value, marked: bool) -> Value {
    if marked && !is_error(&output) {
        json!({"is_error": true, "error": output})
    } else {
        output
    }
}

pub(super) fn denial(call: &ToolCall, middleware: &str, reason: &str) -> MaterializedToolResult {
    MaterializedToolResult::new(
        ToolResult::new(
            call.clone(),
            json!({
                "is_error": true, "error": reason, "blocked_by_middleware": middleware,
            }),
        ),
        InnerToolCallStatus::Blocked,
    )
}

fn children(value: &Value) -> Vec<&Value> {
    match value {
        Value::Object(object) => object.values().collect(),
        Value::Array(values) => values.iter().collect(),
        _ => vec![],
    }
}
