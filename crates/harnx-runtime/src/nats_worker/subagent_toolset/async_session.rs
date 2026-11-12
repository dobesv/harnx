//! Async sub-agent send/wait contracts, durable dispatch and bounded observation.
//! `session_send.wait` and the top-level `session_wait` options share one
//! resolver, and both entry points use the
//! [`super::session_read`] formatter. Discovery is wired separately; synchronous
//! `session_prompt` behavior and schema stay unchanged.
//!
//! Handle semantics: `prompt_id` is the original `AppendedPrompt.user_msg_id`
//! of the dispatched prompt — the durable identity a wait re-attaches to.
//! `invocation_id` is execution identity and stays diagnostic only.

pub(super) mod dispatch;
pub(super) mod observation;
mod specs;

pub(crate) use specs::{session_send_spec, session_wait_spec};

use super::session_read::{ReadOptions, ResolvedRead};
use crate::nats_session_metadata::{CallTimeoutOverride, EffectiveDeadline};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default soft wait in seconds. Applies to an explicit `wait: {}` on send and
/// to an omitted `seconds` on `session_wait`. Omitted `send.wait` remains
/// dispatch-only.
pub(crate) const WAIT_DEFAULT_SECS: u64 = 240;
/// Maximum accepted soft wait in seconds; `forever` lifts it explicitly. It
/// stays under the five-minute prompt-cache lifetime, so a parent that waits
/// the full limit still finds its cache warm when the result comes back.
pub(crate) const WAIT_MAX_SECS: u64 = 240;

/// How long an observation may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitLimit {
    Seconds(u64),
    Forever,
}

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
    pub(crate) forever: Option<bool>,
    #[serde(default)]
    pub(crate) include_partial: Option<bool>,
    #[serde(default)]
    pub(crate) read: Option<ReadOptions>,
}

/// A validated wait request.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedWait {
    /// Soft observation limit.
    pub(crate) limit: WaitLimit,
    /// Whether pending/blocked observations include partial child output.
    /// Terminal output is delivered under the same bound either way.
    pub(crate) include_partial: bool,
    pub(crate) read: ResolvedRead,
}

impl Default for ResolvedWait {
    fn default() -> Self {
        Self {
            limit: WaitLimit::Seconds(WAIT_DEFAULT_SECS),
            include_partial: true,
            read: ResolvedRead::default(),
        }
    }
}

/// Which tool carries the wait options; it decides the out-of-range error text.
#[derive(Debug, Clone, Copy)]
enum WaitCaller {
    Send,
    Wait,
}

impl WaitCaller {
    fn range_error(self) -> String {
        match self {
            Self::Send => format!(
                "wait.seconds must be between 1 and {WAIT_MAX_SECS}; omit wait to return immediately"
            ),
            Self::Wait => format!(
                "seconds must be between 1 and {WAIT_MAX_SECS}; to check status without waiting, \
                 call session_load with prompt_id"
            ),
        }
    }
}

impl WaitOptions {
    /// Resolve the nested `session_send.wait` object.
    pub(crate) fn resolve(&self) -> Result<ResolvedWait, String> {
        self.resolve_for(WaitCaller::Send)
    }

    fn resolve_for(&self, caller: WaitCaller) -> Result<ResolvedWait, String> {
        let forever = self.forever.unwrap_or(false);
        if forever && self.seconds.is_some() {
            return Err("pass either seconds or forever, not both".to_string());
        }
        let limit = if forever {
            WaitLimit::Forever
        } else {
            let seconds = self.seconds.unwrap_or(WAIT_DEFAULT_SECS);
            if !(1..=WAIT_MAX_SECS).contains(&seconds) {
                return Err(caller.range_error());
            }
            WaitLimit::Seconds(seconds)
        };
        let include_partial = self.include_partial.unwrap_or(true);
        let read = self.read.clone().unwrap_or_default().resolve()?;
        Ok(ResolvedWait {
            limit,
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

impl SessionPromptStatus {
    /// Whether durable evidence ended the prompt.
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

/// Whether the dispatched prompt's activation was durably confirmed.
/// `None` means unknown at observation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DispatchConfirmation {
    Confirmed,
    Unconfirmed,
}

/// What ended a wait early with its normal result, other than its own limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitStop {
    /// A new message arrived for the waiting caller.
    NewMessage,
    /// The wait reached the minute it leaves before the caller's run deadline.
    CallerDeadline,
}

impl WaitStop {
    /// The value with the longest serialized name, which result metadata
    /// reserves room for.
    pub(crate) const WIDEST: Self = Self::CallerDeadline;
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
    /// What ended the wait early, when it was not the wait's own limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stopped_by: Option<WaitStop>,
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
        stopped_by: Some(WaitStop::WIDEST),
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

/// `session_wait` arguments. Requires the exact original prompt handle. The
/// wait options sit at the top level; serde cannot combine `flatten` with
/// `deny_unknown_fields`, so they are declared directly.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitArgs {
    pub(crate) session_id: String,
    pub(crate) prompt_id: String,
    #[serde(default)]
    pub(crate) seconds: Option<u64>,
    #[serde(default)]
    pub(crate) forever: Option<bool>,
    #[serde(default)]
    pub(crate) include_partial: Option<bool>,
    #[serde(default)]
    pub(crate) read: Option<ReadOptions>,
}

impl WaitArgs {
    fn wait_options(&self) -> WaitOptions {
        WaitOptions {
            seconds: self.seconds,
            forever: self.forever,
            include_partial: self.include_partial,
            read: self.read.clone(),
        }
    }
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
/// Omitted `seconds` defaults to a 240-second soft wait.
pub(crate) fn parse_wait_args(args: Value) -> Result<(WaitArgs, ResolvedWait), String> {
    let nested_wait = args.get("wait").is_some();
    let parsed: WaitArgs = serde_json::from_value(args).map_err(|error| {
        let hint = if nested_wait {
            "; session_wait takes seconds/forever at the top level, the nested wait object was removed"
        } else {
            ""
        };
        format!("invalid session_wait arguments: {error}{hint}")
    })?;
    if parsed.session_id.trim().is_empty() {
        return Err("session_id must not be empty".to_string());
    }
    if parsed.prompt_id.trim().is_empty() {
        return Err("prompt_id must not be empty".to_string());
    }
    let resolved = parsed.wait_options().resolve_for(WaitCaller::Wait)?;
    ensure_result_metadata_fits(
        &parsed.session_id,
        &parsed.prompt_id,
        resolved.read.max_output_bytes,
    )?;
    Ok((parsed, resolved))
}

#[cfg(test)]
#[path = "async_session_tests.rs"]
mod async_session_tests;
#[cfg(test)]
mod new_message_tests;
