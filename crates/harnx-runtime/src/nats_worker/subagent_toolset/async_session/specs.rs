//! Tool specs and schemas for `session_send` and `session_wait`.

use super::super::session_read::read_object_schema;
use super::{WAIT_DEFAULT_SECS, WAIT_MAX_SECS};
use harnx_toolset::{
    ToolProgressKind, ToolSpec, SUBAGENT_SESSION_SEND_TOOL, SUBAGENT_SESSION_WAIT_TOOL,
};
use serde_json::{json, Value};

// Keep repeated observations compact without changing the model-facing result.
// Errors have no status/output envelope, so retain their diagnostic instead.
const WAIT_RESULT_TEMPLATE: &str = concat!(
    "{% if result.is_error %}",
    "{{ result.error | default(raw(), true) | replace('\r', ' ') | replace('\n', ' ') | truncate(100) }}",
    "{% else %}{{ result.status | default('unknown', true) }}",
    "{% if result.wait_expired %} (wait expired){% endif %}",
    "{% if result.stopped_by %} ({{ result.stopped_by }}){% endif %}",
    "{% if result.output.observation.error and result.output.observation.error != 'soft_wait_expired' %}",
    " — {{ result.output.observation.error }}{% endif %}",
    "{% if result.terminal and result.status != 'completed' %}",
    "{% set diagnostic = result.output.events | default([]) | selectattr('type', 'equalto', 'error') | list | last %}",
    "{% if diagnostic.text %} — {{ diagnostic.text | replace('\r', ' ') | replace('\n', ' ') | truncate(100) }}{% endif %}",
    "{% endif %}{% endif %}",
);

/// Wait properties shared by `session_send.wait` and the top level of
/// `session_wait`.
pub(crate) fn wait_properties() -> serde_json::Map<String, Value> {
    let Value::Object(properties) = json!({
        "seconds": {
            "type": "integer",
            "minimum": 1,
            "maximum": WAIT_MAX_SECS,
            "default": WAIT_DEFAULT_SECS,
            "description": "Soft observation budget in seconds, 1 to 240 \
    (default 240). Expiry returns the partial snapshot; the child keeps running. To \
    check status without waiting, use session_load with prompt_id. Not combinable \
    with forever."
        },
        "forever": {
            "type": "boolean",
            "description": "Wait with no time limit until the prompt settles, \
    the call is cancelled or a new message arrives. The wait never extends the \
    child's hard execution deadline. Not combinable with seconds."
        },
        "include_partial": {
            "type": "boolean",
            "description": "Whether to include partial child output while the \
    prompt is pending or blocked. Default true. Explicit false omits pending/\
    blocked text; terminal output is still returned under the same byte bound."
        },
        "read": json!({"anyOf": [read_object_schema(), {"type": "null"}]})
    }) else {
        unreachable!("wait properties are an object")
    };
    properties
}

/// The `session_send.wait` object schema. Strict: unknown fields are rejected.
pub(crate) fn wait_object_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional soft wait for this prompt. The wait is an \
    observation budget only: it never extends the child's hard execution deadline, \
    and expiry never cancels the child. Omit wait to dispatch and return \
    immediately; use session_wait later to wait, or session_load to check status.",
        "properties": wait_properties(),
        "additionalProperties": false
    })
}

/// Tool spec for `session_send` (agent-prefixed at registration by Task 5).
pub(crate) fn session_send_spec(
    agent: &str,
    policy: Option<&super::super::TargetRunPolicy>,
) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: SUBAGENT_SESSION_SEND_TOOL.to_string(),
        description: format!(
            "Asynchronously send a prompt to the '{agent}' agent and optionally \
soft-wait for its result. Omit session_id (or pass an empty/whitespace value) to \
start a new session with a generated ID. Returns a durable {{session_id, \
prompt_id}} handle immediately; omit wait for dispatch-only, pass wait to soft-wait \
up to 240 seconds here (a new message for you may end the wait early; stopped_by: \"caller_deadline\" means your own run deadline is near, so report now instead of waiting again), use \
session_wait to wait later and session_load to check status without waiting. Sub-agents return \
files by including cid: attachment URLs in their reply. {}",
            super::super::policy_description(policy)
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The prompt message to send to the agent"
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of cid: attachment URLs to pass to the sub-agent"
                },
                "session_id": {
                    "type": "string",
                    "description": "Optional. Omit (or pass an empty/whitespace value) to start a new session with a generated ID; this is the default. To continue an earlier session, pass the exact session_id returned by a prior call. IDs are case-sensitive and local to this agent. Do not invent a session ID."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": super::super::policy_description(policy)
                },
                "wait": json!({"anyOf": [wait_object_schema(), {"type": "null"}]})
            },
            "required": ["message"],
            "additionalProperties": false
        }),
        idempotent_hint: false,
        read_only_hint: false,
        timeout_secs: None,
        meta: None,
    }
    .without_request_timeout()
    .with_call_template(&format!(
        "@ {agent}{{% if args.session_id %}} [{}]{{% endif %}}\n{{{{ args.message }}}}",
        super::super::SHORT_SESSION_ID
    ))
    .with_kind(ToolProgressKind::Execute)
    .with_stop_on_new_message()
}

fn wait_tool_schema() -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "session_id".into(),
        json!({
            "type": "string",
            "description": "The session ID the prompt was dispatched to. \
        Case-sensitive; do not invent."
        }),
    );
    properties.insert(
        "prompt_id".into(),
        json!({
            "type": "string",
            "description": "The prompt_id from the session_send result handle \
        (the original prompt identity, not an invocation ID)."
        }),
    );
    properties.extend(wait_properties());
    json!({
        "type": "object",
        "properties": properties,
        "required": ["session_id", "prompt_id"],
        "additionalProperties": false
    })
}

/// Tool spec for `session_wait` (agent-prefixed at registration by Task 5).
pub(crate) fn session_wait_spec(agent: &str) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: SUBAGENT_SESSION_WAIT_TOOL.to_string(),
        description: format!(
            "Wait (soft) for the result of a prompt previously dispatched to the \
'{agent}' agent with session_send, for up to 240 seconds or until it settles. \
Returns the prompt's durable observation: status, terminal flag and bounded \
output with continuation metadata. A new message for you may end the wait early with \
stopped_by: \"new_message\". stopped_by: \"caller_deadline\" means your own \
run deadline is near: report now instead of waiting again. Repeated waits recover the same result and \
never consume it. The wait never cancels the child or extends its execution \
deadline. To check status without waiting, use session_load with prompt_id."
        ),
        input_schema: wait_tool_schema(),
        idempotent_hint: false,
        read_only_hint: false,
        timeout_secs: None,
        meta: None,
    }
    .without_request_timeout()
    .with_call_template(&format!(
        "@ {agent} [{}] wait {{{{ args.prompt_id | truncate(8, '') }}}}",
        super::super::SHORT_SESSION_ID
    ))
    .with_result_template(WAIT_RESULT_TEMPLATE)
    .with_kind(ToolProgressKind::Read)
    .with_stop_on_new_message()
}

#[cfg(test)]
#[path = "specs_tests.rs"]
mod tests;
