//! Async sub-agent send/wait contracts, durable dispatch and bounded observation.
//! Both observation entry points share the nested `wait` options and
//! [`super::session_read`] formatter. Discovery is wired separately; synchronous
//! `session_prompt` behavior and schema stay unchanged.
//!
//! Handle semantics: `prompt_id` is the original `AppendedPrompt.user_msg_id`
//! of the dispatched prompt — the durable identity a wait re-attaches to.
//! `invocation_id` is execution identity and stays diagnostic only.

pub(super) mod dispatch;
pub(super) mod observation;

use super::session_read::{read_object_schema, ReadOptions, ResolvedRead};
use crate::nats_session_metadata::{CallTimeoutOverride, EffectiveDeadline};
use harnx_toolset::{
    ToolProgressKind, ToolSpec, SUBAGENT_SESSION_SEND_TOOL, SUBAGENT_SESSION_WAIT_TOOL,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Default soft wait: 120 seconds, kept below the common five-minute LLM
/// cache TTL window. Applies to explicit `wait: {}` on send and to an omitted
/// `wait` on `session_wait`. Omitted `send.wait` remains dispatch-only.
pub(crate) const DEFAULT_WAIT_SECONDS: u64 = 120;
/// Maximum accepted soft wait. Caller choice only; never recommended.
pub(crate) const MAX_WAIT_SECONDS: u64 = 300;

/// Soft-observation options shared by `session_send.wait` and `session_wait`.
/// All fields optional; `null` is equivalent to omission. The wait budget is
/// observation-only: it never extends, renews or replaces the child's
/// inherited hard execution deadline.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitOptions {
    #[serde(default)]
    pub(crate) seconds: Option<u64>,
    #[serde(default)]
    pub(crate) include_partial: Option<bool>,
    #[serde(default)]
    pub(crate) read: Option<ReadOptions>,
}

/// A validated `wait` object.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedWait {
    /// Soft observation budget. Zero permits one bounded snapshot operation,
    /// no repair/retry/wait loop.
    pub(crate) seconds: u64,
    /// Whether pending/blocked observations include partial child output.
    /// Terminal output is delivered under the same bound either way.
    pub(crate) include_partial: bool,
    pub(crate) read: ResolvedRead,
}

impl Default for ResolvedWait {
    fn default() -> Self {
        Self {
            seconds: DEFAULT_WAIT_SECONDS,
            include_partial: true,
            read: ResolvedRead::default(),
        }
    }
}

impl WaitOptions {
    pub(crate) fn resolve(&self) -> Result<ResolvedWait, String> {
        let seconds = self.seconds.unwrap_or(DEFAULT_WAIT_SECONDS);
        if seconds > MAX_WAIT_SECONDS {
            return Err(format!("wait.seconds must be at most {MAX_WAIT_SECONDS}"));
        }
        let include_partial = self.include_partial.unwrap_or(true);
        let read = self.read.clone().unwrap_or_default().resolve()?;
        Ok(ResolvedWait {
            seconds,
            include_partial,
            read,
        })
    }
}

/// Durable, prompt-specific handle for waiting on a dispatched prompt.
/// `prompt_id` is the original `AppendedPrompt.user_msg_id`; repeated waits
/// recover the same result without consuming it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PromptHandle {
    pub(crate) session_id: String,
    pub(crate) prompt_id: String,
}

/// Observation status of a dispatched prompt. Only durable evidence
/// establishes a terminal state; approval is nonterminal `blocked`, and a
/// deadline elapsed without terminal evidence is not `timed_out`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionPromptStatus {
    Pending,
    Blocked,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Unknown,
}

/// Whether the dispatched prompt's activation was durably confirmed.
/// `None` means unknown at observation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DispatchConfirmation {
    Confirmed,
    Unconfirmed,
}

/// Shared send/wait result envelope. `output` carries the bounded read (same
/// formatter for final replies and partial output) and is sized so the entire
/// serialized envelope fits the resolved `max_output_bytes` budget; the
/// envelope's own metadata is reserved first so a dispatched handle can never
/// be lost to the output formatter.
#[derive(Debug, Serialize)]
pub(crate) struct SessionPromptResult {
    pub(crate) session_id: String,
    pub(crate) prompt_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) invocation_id: Option<String>,
    pub(crate) dispatch: Option<DispatchConfirmation>,
    pub(crate) status: SessionPromptStatus,
    pub(crate) terminal: bool,
    pub(crate) wait_expired: bool,
    pub(crate) elapsed_ms: u64,
    pub(crate) output: Value,
}

/// Serialized size of the result envelope with empty output — the bytes that
/// must be reserved before fitting any output content.
pub(crate) fn result_metadata_bytes(session_id: &str, prompt_id: &str) -> usize {
    let envelope = SessionPromptResult {
        session_id: session_id.to_string(),
        prompt_id: prompt_id.to_string(),
        invocation_id: None,
        dispatch: None,
        status: SessionPromptStatus::Unknown,
        terminal: false,
        wait_expired: false,
        elapsed_ms: u64::MAX,
        output: Value::Null,
    };
    serde_json::to_string(&envelope)
        .expect("result envelope serializes")
        .len()
}

/// Reject identifiers that cannot fit their own handle metadata before any
/// dispatch side effect, so a dispatched handle is never lost to the output
/// budget.
pub(crate) fn ensure_result_metadata_fits(
    session_id: &str,
    prompt_id: &str,
    max_output_bytes: usize,
) -> Result<(), String> {
    let reserved = result_metadata_bytes(session_id, prompt_id);
    if reserved > max_output_bytes {
        Err(format!(
            "result metadata ({reserved} bytes for session_id and prompt_id) exceeds \
             max_output_bytes budget {max_output_bytes}; the handle could not be returned"
        ))
    } else {
        Ok(())
    }
}

/// `session_send` arguments. Message/attachments/session_id/timeout_secs keep
/// `session_prompt` semantics; `timeout_secs` stays the hard child execution
/// allowance (omitted/null/nonpositive inherits target policy). Omitted or
/// null `wait` is dispatch-only.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendArgs {
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) attachments: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "harnx_core::config_data::deserialize_timeout_override"
    )]
    pub(crate) timeout_secs: Option<u64>,
    #[serde(default)]
    pub(crate) wait: Option<WaitOptions>,
    #[serde(default, rename = "__harnx_parent_session_id")]
    pub(crate) parent_session_id: Option<String>,
    #[serde(default, rename = "__harnx_tool_call_id")]
    pub(crate) tool_call_id: Option<String>,
}

/// `session_wait` arguments. Requires the exact original prompt handle.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitArgs {
    pub(crate) session_id: String,
    pub(crate) prompt_id: String,
    #[serde(default)]
    pub(crate) wait: Option<WaitOptions>,
}

/// Parse and fully validate `session_send` arguments before any side effect.
/// Returns the resolved soft wait when one was requested; `None` means
/// dispatch-only (omitted or null `wait`).
pub(crate) fn parse_send_args(args: Value) -> Result<(SendArgs, Option<ResolvedWait>), String> {
    let parsed: SendArgs = serde_json::from_value(args)
        .map_err(|error| format!("invalid session_send arguments: {error}"))?;
    // Same deadline rules as session_prompt: reject impossible overrides
    // before dispatch. The soft wait never grants execution time.
    EffectiveDeadline::resolve(
        Default::default(),
        None,
        CallTimeoutOverride::from_optional(parsed.timeout_secs),
        None,
        chrono::Utc::now(),
    )
    .map_err(|error| format!("invalid session_send arguments: {error}"))?;
    let resolved_wait = parsed.wait.as_ref().map(WaitOptions::resolve).transpose()?;
    // The result envelope always returns the handle, so pre-dispatch the
    // identifiers must fit their own metadata under the output budget the
    // caller asked for (default budget when dispatch-only).
    let budget = resolved_wait
        .as_ref()
        .map(|wait| wait.read.max_output_bytes)
        .unwrap_or(super::session_read::DEFAULT_MAX_OUTPUT_BYTES as usize);
    if let Some(session_id) = parsed.session_id.as_deref() {
        ensure_result_metadata_fits(session_id, "", budget)?;
    }
    Ok((parsed, resolved_wait))
}

/// Parse and fully validate `session_wait` arguments before any side effect.
/// An omitted `wait` defaults to a 120-second soft wait.
pub(crate) fn parse_wait_args(args: Value) -> Result<(WaitArgs, ResolvedWait), String> {
    let parsed: WaitArgs = serde_json::from_value(args)
        .map_err(|error| format!("invalid session_wait arguments: {error}"))?;
    if parsed.session_id.trim().is_empty() {
        return Err("session_id must not be empty".to_string());
    }
    if parsed.prompt_id.trim().is_empty() {
        return Err("prompt_id must not be empty".to_string());
    }
    let resolved = parsed
        .wait
        .as_ref()
        .map_or_else(|| Ok(ResolvedWait::default()), WaitOptions::resolve)?;
    ensure_result_metadata_fits(
        &parsed.session_id,
        &parsed.prompt_id,
        resolved.read.max_output_bytes,
    )?;
    Ok((parsed, resolved))
}

/// The shared `wait` object schema. Nested and strict: unknown fields are
/// rejected at every level.
pub(crate) fn wait_object_schema() -> Value {
    json!({
        "type": "object",
        "description": "Optional soft wait for this prompt. The seconds are an \
    observation budget only — they never extend the child's hard execution \
    deadline, and expiry never cancels the child. Omitted or null means no wait \
    for session_send (dispatch-only); session_wait defaults an omitted wait to \
    120 seconds.",
        "properties": {
            "seconds": {
                "type": "integer",
                "minimum": 0,
                "maximum": 300,
                "default": 120,
                "description": "Soft observation budget in seconds. Default 120, \
    maximum 300. Zero takes one immediate bounded snapshot. Expiry returns the \
    partial snapshot; the child keeps running."
            },
            "include_partial": {
                "type": "boolean",
                "description": "Whether to include partial child output while the \
    prompt is pending or blocked. Default true. Explicit false omits pending/\
    blocked text; terminal output is still returned under the same byte bound."
            },
            "read": json!({"anyOf": [read_object_schema(), {"type": "null"}]})
        },
        "additionalProperties": false
    })
}

/// Tool spec for `session_send` (agent-prefixed at registration by Task 5).
pub(crate) fn session_send_spec(agent: &str, policy: Option<&super::TargetRunPolicy>) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: SUBAGENT_SESSION_SEND_TOOL.to_string(),
        description: format!(
            "Asynchronously send a prompt to the '{agent}' agent and optionally \
soft-wait for its result. Omit session_id (or pass an empty/whitespace value) to \
start a new session with a generated ID. Returns a durable {{session_id, \
prompt_id}} handle immediately; wait with the agent's session_wait tool to \
observe the result, or omit wait entirely for dispatch-only. Sub-agents return \
files by including cid: attachment URLs in their reply. {}",
            super::policy_description(policy)
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
                    "description": super::policy_description(policy)
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
        super::SHORT_SESSION_ID
    ))
    .with_kind(ToolProgressKind::Execute)
}

/// Tool spec for `session_wait` (agent-prefixed at registration by Task 5).
pub(crate) fn session_wait_spec(agent: &str) -> ToolSpec {
    ToolSpec {
        cancellation_guarantee: Default::default(),
        name: SUBAGENT_SESSION_WAIT_TOOL.to_string(),
        description: format!(
            "Wait (soft) for the result of a prompt previously dispatched to the \
'{agent}' agent with session_send. Returns the prompt's durable observation: \
status, terminal flag and bounded output with continuation metadata. Repeated \
waits recover the same result and never consume it. The wait never cancels \
the child or extends its execution deadline."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID the prompt was dispatched to. \
Case-sensitive; do not invent."
                },
                "prompt_id": {
                    "type": "string",
                    "description": "The prompt_id from the session_send result handle \
(the original prompt identity, not an invocation ID)."
                },
                "wait": json!({"anyOf": [wait_object_schema(), {"type": "null"}]})
            },
            "required": ["session_id", "prompt_id"],
            "additionalProperties": false
        }),
        idempotent_hint: false,
        read_only_hint: false,
        timeout_secs: None,
        meta: None,
    }
    .without_request_timeout()
    .with_call_template(&format!(
        "@ {agent} [{}] wait\nsession_id: {{{{ args.session_id }}}}\nprompt_id: {{{{ args.prompt_id }}}}",
        super::SHORT_SESSION_ID
    ))
    .with_kind(ToolProgressKind::Read)
}

#[cfg(test)]
#[path = "async_session_tests.rs"]
mod async_session_tests;
