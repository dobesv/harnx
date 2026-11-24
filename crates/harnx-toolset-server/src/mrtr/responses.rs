//! Answer-map encoding: harnx `ToolInputResponse` ↔ MRTR `inputResponses`.
//!
//! Responses travel as `inputResponses` map values. To stay conformant for
//! third-party servers, harnx always answers elicitation requests with an
//! `ElicitResult`-shaped object (`action: accept` / `decline`, optional
//! `content`); the harnx answer's action name and note ride in `_meta` so a
//! harnx↔harnx hop loses nothing.

use std::collections::BTreeMap;

use harnx_toolset::ToolInputResponse;
use rmcp::model::InputResponses;
use serde_json::{json, Map, Value};

/// `ElicitResult._meta` key carrying the harnx answer's action name.
pub const HARNX_ACTION_META_KEY: &str = "harnx:action";
/// `ElicitResult._meta` key carrying the harnx answer's operator note.
pub const HARNX_NOTE_META_KEY: &str = "harnx:note";

/// harnx answer actions accepted in the bare `ToolInputResponse` shape.
/// Confirm answers are `approve`/`deny`, A2UI form submits are `submit`;
/// custom A2UI button events and everything else ride the ElicitResult
/// `_meta` copy instead, so this set stays closed.
const HARNX_ACTIONS: [&str; 3] = ["approve", "deny", "submit"];

/// ElicitResult actions from the MCP elicitation spec.
const ELICIT_RESULT_ACTIONS: [&str; 3] = ["accept", "decline", "cancel"];

/// Encode harnx answers as MRTR `inputResponses` values.
///
/// `deny` becomes a decline; every other action becomes an accept whose
/// `content` is the answer's data, with the original action name and note
/// preserved in `_meta` for harnx servers.
pub fn responses_to_mrtr(responses: &BTreeMap<String, ToolInputResponse>) -> InputResponses {
    responses
        .iter()
        .map(|(key, response): (_, &ToolInputResponse)| {
            (key.clone(), elicit_result_from_response(response))
        })
        .collect()
}

/// Decode MRTR `inputResponses` into harnx answers.
///
/// Each value is either the harnx `ToolInputResponse` shape or an
/// `ElicitResult`-shaped object; a value that matches neither is an error
/// naming the key.
pub fn responses_from_mrtr(
    responses: &InputResponses,
) -> Result<BTreeMap<String, ToolInputResponse>, String> {
    responses
        .iter()
        .map(|(key, value)| {
            let decoded = response_from_elicit_result(key, value).map_err(|error| {
                format!("inputResponses entry '{key}' is not a usable elicitation answer: {error}")
            })?;
            Ok((key.clone(), decoded))
        })
        .collect()
}

fn elicit_result_from_response(response: &ToolInputResponse) -> Value {
    let mut meta = Map::new();
    meta.insert(
        HARNX_ACTION_META_KEY.to_string(),
        Value::String(response.action.clone()),
    );
    if let Some(note) = &response.note {
        meta.insert(HARNX_NOTE_META_KEY.to_string(), Value::String(note.clone()));
    }
    if response.action == "deny" {
        return json!({"action": "decline", "_meta": meta});
    }
    json!({
        "action": "accept",
        "content": Value::Object(response.data.clone()),
        "_meta": meta,
    })
}

fn response_from_elicit_result(key: &str, value: &Value) -> Result<ToolInputResponse, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "expected an ElicitResult object".to_string())?;
    let action = object
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing action".to_string())?;
    // The two wire shapes share the `action` field: the harnx
    // `ToolInputResponse` and the MCP ElicitResult. Tell them apart by
    // shape: `content` or a protocol accept/decline/cancel action means
    // ElicitResult; a bare known harnx action means the harnx shape.
    // Anything else matches neither shape and fails, so a misnamed action
    // can never become a silent success.
    let elicit_shaped = object.contains_key("content") || ELICIT_RESULT_ACTIONS.contains(&action);
    if !elicit_shaped && HARNX_ACTIONS.contains(&action) {
        return serde_json::from_value(value.clone())
            .map_err(|error| format!("elicitation answer for '{key}' is malformed: {error}"));
    }
    let mut data = object
        .get("content")
        .and_then(|content| content.as_object())
        .cloned()
        .unwrap_or_default();
    let mut note = None;
    let mut resolved = match action {
        "accept" => "approve".to_string(),
        "decline" | "cancel" => "deny".to_string(),
        other => return Err(format!("unknown elicitation action '{other}'")),
    };
    // Prefer the harnx answer copy in _meta, keeping button action names and
    // the operator note over the accept/decline approximation.
    if let Some(meta) = object.get("_meta").and_then(Value::as_object) {
        if let Some(harnx_action) = meta.get(HARNX_ACTION_META_KEY).and_then(Value::as_str) {
            resolved = harnx_action.to_string();
            if harnx_action == "deny" {
                data.clear();
            }
        }
        if let Some(harnx_note) = meta.get(HARNX_NOTE_META_KEY).and_then(Value::as_str) {
            note = Some(harnx_note.to_string());
        }
    }
    let response = ToolInputResponse {
        action: resolved,
        data,
        note,
    };
    // Round-trip through serde to apply the same strictness as the wire type.
    let encoded = serde_json::to_value(&response).map_err(|error| error.to_string())?;
    serde_json::from_value(encoded)
        .map_err(|error| format!("elicitation answer for '{key}' is malformed: {error}"))
}
