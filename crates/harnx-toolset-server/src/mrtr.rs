//! Mapping between harnx tool challenges (`ToolInputRequired`, MRTR
//! SEP-2322) and rmcp's MCP `InputRequiredResult` / `InputResponses`.
//!
//! Both directions live in this module because both sides of an MCP hop can
//! meet a harnx peer: the MCP bridge is the client of arbitrary third-party
//! MRTR servers, and harnx tool servers and the harnx MCP server are the
//! servers behind one. Shared code keeps the two views of the same wire
//! identical.
//!
//! Layout: this file holds the challenge and request-state mapping;
//! [`schema`] derives elicitation schemas from A2UI surfaces and back;
//! [`responses`] encodes and decodes the answer maps.
//!
//! # Conventions
//!
//! A harnx challenge is a single interaction (R9: one prompt at a time), so
//! it always maps to exactly one `inputRequests` entry. The entry is always
//! an `elicitation/create` request, whatever the harnx kind:
//!
//! | harnx side | MRTR side |
//! |---|---|
//! | `confirm` message | Elicitation with an empty `requestedSchema` |
//! | `a2ui` surface inputs | Elicitation `requestedSchema` properties: string → text field, enum → choice picker |
//! | tool `request_state` (any JSON value) | `requestState`, JSON-encoded opaque string |
//! | challenge key | the `inputRequests` map key |
//!
//! `sampling/createMessage`, `roots/list` and URL-form elicitation have no
//! harnx equivalent in Phase 4; the bridge rejects them with a clear error
//! result instead of a silent success.
//!
//! Responses travel as `inputResponses` map values. To stay conformant for
//! third-party servers, the bridge always answers elicitation requests with
//! an `ElicitResult`-shaped object (`action: accept` / `decline`, optional
//! `content`). The harnx answer's action name and note ride in the result's
//! `_meta` under `harnx:action` / `harnx:note`, which harnx servers prefer
//! (full fidelity for harnx↔harnx hops) and third-party servers ignore.
//! Likewise, the server's `InputRequiredResult` carries the full harnx
//! challenge in its `_meta` under `harnx:input_required`, so an a2ui surface
//! and button names survive a harnx↔harnx round trip; the bridge prefers it
//! when present.

mod responses;
mod schema;

pub use responses::{
    responses_from_mrtr, responses_to_mrtr, HARNX_ACTION_META_KEY, HARNX_NOTE_META_KEY,
};
use rmcp::model::{InputRequest, InputRequests, InputRequiredResult, MetaObject};
use serde_json::{Map, Value};

#[cfg(test)]
mod tests;

/// `_meta` key carrying the full harnx challenge on the MRTR result.
pub const HARNX_INPUT_REQUIRED_META_KEY: &str = "harnx:input_required";

/// The harnx tool input round cap tracks rmcp's MRTR client default, so a
/// harnx caller suspends no more rounds than any other Tier 1 client would
/// drive before giving up.
const _: () = assert!(harnx_toolset::MAX_TOOL_INPUT_ROUNDS == rmcp::model::DEFAULT_MRTR_MAX_ROUNDS);

/// Encode one harnx challenge as an MRTR `input_required` result.
///
/// Errors name the offending field; the server surfaces them as a visible
/// recoverable error result rather than a broken MRTR response.
pub fn challenge_to_mrtr(
    input: &harnx_toolset::ToolInputRequired,
) -> Result<InputRequiredResult, String> {
    // Check the state first: this function's caller surfaces the error on the
    // MCP side, so it names the MRTR field (`requestState`), not the harnx
    // `request_state` the shared `validate()` would name.
    let request_state = request_state_from_value(&input.request_state)?;
    input
        .validate()
        .map_err(|error| format!("cannot map input_required challenge to MRTR: {error}"))?;
    // One elicitation request for both kinds: a confirm is an empty form,
    // an a2ui derives its schema from the surface.
    let (message, requested_schema) = match input.kind.as_str() {
        harnx_toolset::ToolInputRequired::KIND_CONFIRM => (
            input
                .message
                .clone()
                .ok_or_else(|| "confirm challenge has no message".to_string())?,
            schema::empty_requested_schema(),
        ),
        harnx_toolset::ToolInputRequired::KIND_A2UI => {
            let summary = input.summary.trim();
            (
                if summary.is_empty() {
                    "Provide the requested input".to_string()
                } else {
                    summary.to_string()
                },
                schema::requested_schema_from_surface(input.surface.as_ref())?,
            )
        }
        other => return Err(format!("cannot map input_required kind '{other}' to MRTR")),
    };
    let mut requests = InputRequests::new();
    requests.insert(
        input.key.clone(),
        InputRequest::Elicitation(rmcp::model::ElicitRequest::new(
            rmcp::model::ElicitRequestParams::FormElicitationParams {
                meta: None,
                message,
                requested_schema,
            },
        )),
    );
    Ok(InputRequiredResult::new(Some(requests), request_state)
        .with_meta(harnx_challenge_meta(input)))
}

/// Decode an MRTR `input_required` result into a harnx challenge.
///
/// The `inputRequests` map key is authoritative: responses on the rerun are
/// keyed by it, so it becomes the harnx challenge key even when the harnx
/// `_meta` copy disagrees.
pub fn challenge_from_mrtr(
    result: &InputRequiredResult,
) -> Result<harnx_toolset::ToolInputRequired, String> {
    let (key, request) = single_request(result)?;
    let request_state = request_state_from_mrtr(result.request_state.as_deref())?;
    if let Some(meta) = result
        .meta
        .as_ref()
        .and_then(|meta| meta.0.get(HARNX_INPUT_REQUIRED_META_KEY))
    {
        // Prefer the harnx challenge copy: the a2ui surface, summary and
        // message survive exactly, not through the lossy schema derivation.
        let mut input: harnx_toolset::ToolInputRequired = serde_json::from_value(meta.clone())
            .map_err(|error| format!("invalid {HARNX_INPUT_REQUIRED_META_KEY} meta: {error}"))?;
        input.key = key;
        input.request_state = request_state;
        input
            .validate()
            .map_err(|error| format!("harnx challenge in MRTR result is invalid: {error}"))?;
        return Ok(input);
    }
    match request {
        rmcp::model::InputRequest::Elicitation(elicit) => match &elicit.params {
            rmcp::model::ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                if requested_schema.properties.is_empty() {
                    Ok(harnx_toolset::ToolInputRequired::confirm(
                        key, message, request_state,
                    ))
                } else {
                    let surface =
                        schema::surface_from_requested_schema(message, &requested_schema.properties)?;
                    Ok(harnx_toolset::ToolInputRequired::a2ui(
                        key, surface, message, request_state,
                    ))
                }
            }
            rmcp::model::ElicitRequestParams::UrlElicitationParams { .. } => Err(format!(
                "MRTR input request '{key}' is URL elicitation, which harnx cannot show as an interaction"
            )),
            _ => Err(format!(
                "MRTR input request '{key}' is an elicitation kind harnx cannot show as an interaction"
            )),
        },
        rmcp::model::InputRequest::CreateMessage(_) => Err(format!(
            "MRTR input request '{key}' needs sampling (createMessage), which harnx cannot fulfill"
        )),
        rmcp::model::InputRequest::ListRoots(_) => Err(format!(
            "MRTR input request '{key}' needs the client's roots list, which harnx cannot fulfill"
        )),
        // rmcp marks InputRequest non-exhaustive: future kinds are refused,
        // not silently dropped.
        _ => Err(format!(
            "MRTR input request '{key}' is a kind harnx cannot fulfill"
        )),
    }
}

/// The one input request a harnx challenge can be built from, with its map
/// key and the guards the MRTR shape owes the caller.
fn single_request(result: &InputRequiredResult) -> Result<(String, &InputRequest), String> {
    let missing = "MRTR input_required result without inputRequests cannot be shown to the user";
    let requests = result
        .input_requests
        .as_ref()
        .ok_or_else(|| missing.to_string())?;
    let count = requests.len();
    let (key, request) = requests.iter().next().ok_or_else(|| missing.to_string())?;
    if count > 1 {
        return Err(format!(
            "MRTR input_required result has {count} input requests; harnx shows one interaction at a time"
        ));
    }
    if key.is_empty() || key.len() > harnx_toolset::MAX_TOOL_INPUT_KEY_BYTES {
        return Err(format!(
            "MRTR input request key exceeds {} bytes",
            harnx_toolset::MAX_TOOL_INPUT_KEY_BYTES
        ));
    }
    Ok((key.clone(), request))
}

/// Convert the tool's `request_state` value into the MRTR `requestState`
/// string to echo on the re-call.
///
/// Null means no state. A string passes through unchanged: it may be the
/// opaque text a third-party server sent originally, and re-encoding it
/// would hand the server different bytes than it asked to have echoed.
/// Any other value is compact JSON, matching how harnx servers encode
/// their state. The result is bounded by
/// [`harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES`] either way.
pub fn request_state_from_value(state: &Value) -> Result<Option<String>, String> {
    let encoded = match state {
        Value::Null => return Ok(None),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other)
            .map_err(|error| format!("request_state cannot be echoed over MCP: {error}"))?,
    };
    if encoded.len() > harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES {
        return Err(format!(
            "requestState exceeds {} bytes",
            harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES
        ));
    }
    Ok(Some(encoded))
}

/// Decode an MRTR `requestState` string back into the tool's JSON state.
///
/// The string is opaque per the spec; harnx servers encode JSON in it. Text
/// that is not valid JSON is passed through as the string itself: the value
/// is an echo of something this server produced, and a challenge must never
/// become a protocol error over it.
pub fn request_state_from_mrtr(state: Option<&str>) -> Result<Value, String> {
    match state {
        None => Ok(Value::Null),
        Some(state) => {
            if state.len() > harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES {
                return Err(format!(
                    "MRTR requestState exceeds {} bytes",
                    harnx_toolset::MAX_TOOL_REQUEST_STATE_BYTES
                ));
            }
            Ok(serde_json::from_str(state).unwrap_or_else(|_| Value::String(state.to_string())))
        }
    }
}

fn harnx_challenge_meta(input: &harnx_toolset::ToolInputRequired) -> MetaObject {
    let mut meta = Map::new();
    let value = serde_json::to_value(input).unwrap_or(Value::Null);
    meta.insert(HARNX_INPUT_REQUIRED_META_KEY.to_string(), value);
    MetaObject(meta)
}
