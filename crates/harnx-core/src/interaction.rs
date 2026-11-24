//! Modal tool interactions. Responses are runtime-owned input, never tool arguments.
//!
//! These types are readers-only until durable suspension writers are deployed.
//! Checkpoint references name private storage; they never contain execution observations.

pub mod non_modal;
pub mod response;
pub mod surface;
pub mod text;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// One logical `next` traversal from an outer middleware frame toward the issuer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionStep {
    pub middleware: String,
    pub key: String,
}

/// Host-created request. Scripts cannot request suspension by printing this JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionRequest {
    pub interaction_id: String,
    /// `middleware:<resolved name>` (later, `tool:<server>/<tool>`).
    pub producer: String,
    pub step_path: Vec<InteractionStep>,
    pub key: String,
    pub kind: String,
    pub payload: Value,
    pub summary: String,
    pub chain_version: String,
}

/// Accepted user action. Only the durable response acceptance path may supply it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionResponse {
    pub action: String,
    pub data: serde_json::Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// All identity inputs, including the session storage key, not its bare local ID.
/// Explicit keys are required when scripts change traversal order across reruns.
#[derive(Debug, Clone)]
pub struct InteractionIdentity<'a> {
    pub session: &'a str,
    pub tool_round_seq: u64,
    pub tool_call_id: &'a str,
    pub chain_version: &'a str,
    pub producer: &'a str,
    pub step_path: &'a [InteractionStep],
    pub key: &'a str,
    pub kind: &'a str,
    pub payload: &'a Value,
}

/// Stable, versioned SHA-256 identity. JSON framing prevents delimiter collisions.
pub fn interaction_id(identity: &InteractionIdentity<'_>) -> String {
    let mut input = json!([
        "harnx-interaction-v1",
        identity.session,
        identity.tool_round_seq,
        identity.tool_call_id,
        identity.chain_version,
        identity.producer,
        identity.step_path,
        identity.key,
        identity.kind,
        canonical_interaction_payload(identity.payload),
    ]);
    input.sort_all_objects();
    let bytes = serde_json::to_vec(&input).expect("interaction identity contains only JSON values");
    crate::crypto::hex_encode(&Sha256::digest(bytes))
}

/// Ignore transport-generated identity/time, not user data or action semantics.
/// In particular `context`, `data`, component IDs, and visible text stay intact.
/// Phase 4 must extend this allowlist alongside its pinned A2UI schema.
pub fn canonical_interaction_payload(payload: &Value) -> Value {
    let mut canonical = payload.clone();
    remove_transport_fields(&mut canonical);
    if let Some(surface) = canonical.get_mut("createSurface") {
        remove_transport_fields(surface);
        if let Some(components) = surface.get_mut("components").and_then(Value::as_array_mut) {
            for component in components {
                if let Some(action) = component.get_mut("action") {
                    remove_transport_fields(action);
                }
            }
        }
    }
    if let Some(action) = canonical.get_mut("action") {
        remove_transport_fields(action);
    }
    canonical.sort_all_objects();
    canonical
}

fn remove_transport_fields(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("surfaceId");
        object.remove("timestamp");
    }
}

/// Fixed Phase 2 surface. Clients recognize `harnx.kind` and use their native
/// approve/deny UI; this is not a general A2UI catalog or schema validator.
pub fn confirm_surface(surface_id: &str, message: &str) -> Value {
    json!({
        "harnx": { "kind": "confirm" },
        "createSurface": {
            "surfaceId": surface_id,
            "components": [
                { "id": "title", "component": "Text", "text": message },
                { "id": "approve", "component": "Button", "text": "Approve",
                  "action": { "name": "approve" } },
                { "id": "deny", "component": "Button", "text": "Deny",
                  "action": { "name": "deny" } }
            ]
        }
    })
}

#[cfg(test)]
mod tests;
