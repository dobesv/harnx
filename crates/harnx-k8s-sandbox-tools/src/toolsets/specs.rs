//! Tool surface declarations for the lifecycle toolset: argument parsing and
//! the `sandbox_connect` / `sandbox_status` / `sandbox_release` specs.

use harnx_toolset::{ToolInvokeError, ToolProgressKind, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};

pub(super) fn parse_args<T: for<'de> Deserialize<'de>>(
    tool: &str,
    args: Value,
) -> Result<T, ToolInvokeError> {
    serde_json::from_value(args)
        .map_err(|error| ToolInvokeError::Recoverable(format!("invalid {tool} arguments: {error}")))
}

pub(super) fn lifecycle_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "connect".to_string(),
            description: "Connect this Harnx session to an existing Kubernetes sandbox, the session's bound sandbox, or a newly created one. If the bound sandbox never becomes usable, attach elsewhere with an explicit sandbox_id or clear it with sandbox_release (destroy: true).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sandbox_id": {"type": "string", "description": "Existing sandbox ID; omit to reuse the session's bound sandbox, creating one when none is bound or the bound sandbox no longer exists. Supplying it also escapes a bound sandbox that never activates."},
                    "description": {"type": "string"},
                    "repos": {"type": "array", "items": {"type": "object", "properties": {
                        "repo_url": {"type": "string"}, "branch": {"type": "string"}, "path": {"type": "string"}
                    }, "required": ["repo_url"]}}
                }
            }),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: Some(0),
            meta: None,
        }
        .with_kind(ToolProgressKind::Execute),
        ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "status".to_string(),
            description: "Observe sandbox status without waking it or extending its activity.".to_string(),
            input_schema: json!({"type": "object", "properties": {
                "sandbox_id": {"type": "string"},
                "timeout_secs": {"type": "integer", "minimum": 0}
            }}),
            idempotent_hint: true,
            read_only_hint: true,
            timeout_secs: Some(0),
            meta: None,
        }
        .with_kind(ToolProgressKind::Read),
        ToolSpec {
            cancellation_guarantee: Default::default(),
            name: "release".to_string(),
            description: "Hibernate the current sandbox while preserving storage, or permanently destroy it. Destroying also clears the session's binding, which escapes a bound sandbox that never activates.".to_string(),
            input_schema: json!({"type": "object", "properties": {
                "sandbox_id": {"type": "string"},
                "destroy": {"type": "boolean", "default": false}
            }}),
            idempotent_hint: false,
            read_only_hint: false,
            timeout_secs: Some(0),
            meta: None,
        }
        .with_kind(ToolProgressKind::Delete),
    ]
}
