//! Control metadata comes from materialized results, never from output JSON.
use harnx_core::tool::{SwitchAgentData, ToolResult};
use serde_json::Value;

pub(crate) fn is_error(output: &Value) -> bool {
    ["is_error", "isError"]
        .iter()
        .any(|key| output.get(key).and_then(Value::as_bool) == Some(true))
}

pub(crate) fn effective_switch(result: &ToolResult) -> Option<SwitchAgentData> {
    if is_error(&result.output) {
        return None;
    }
    let mut switch = result.switch_agent.clone()?;
    // A ref may come from an inner call. Dispatch identity belongs to the outer
    // round slot, not to a provider-supplied or script-visible JSON field.
    switch.tool_call_id = result.call.id.clone();
    Some(switch)
}

pub(crate) fn select(results: &[ToolResult]) -> Option<SwitchAgentData> {
    results.iter().find_map(effective_switch)
}

#[cfg(test)]
#[path = "handoff_result_tests.rs"]
mod tests;
