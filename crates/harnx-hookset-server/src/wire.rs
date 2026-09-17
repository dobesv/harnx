//! Both halves of one hook request, in one place.
//!
//! A request names the session and call it belongs to, so a control message
//! can find and cancel it while it runs. A reply is exactly one of three
//! things: the hook's outcome, the interruption that stopped it, or the
//! server's reason for refusing it. The server never leaves a request
//! unanswered — a caller that hears nothing can only time out, and cannot
//! tell a refused request from a lost one.

use anyhow::{Context, Result};
use harnx_core::hooks::HookOutcome;
use harnx_toolset::InterruptedCall;

/// Session the hook call belongs to. A session-scoped cancel matches on it.
pub const HOOK_SESSION_HEADER: &str = "Harnx-Hook-Session";
/// Call id, unique per request, that a control message cancels by.
pub const HOOK_CALL_HEADER: &str = "Harnx-Hook-Call";

/// Headers every hook request must carry.
pub fn hook_request_headers(session_id: &str, call_id: &str) -> async_nats::HeaderMap {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HOOK_SESSION_HEADER, session_id);
    headers.insert(HOOK_CALL_HEADER, call_id);
    headers
}

/// The hook ran to completion.
pub(crate) fn completed_reply(outcome: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "outcome": outcome })
}

/// A control message stopped the hook before it finished.
pub(crate) fn interrupted_reply(interrupted: InterruptedCall) -> serde_json::Value {
    serde_json::json!({ "interrupted": interrupted })
}

/// The server would not run the hook at all. Sent instead of dropping the
/// request, so the caller learns the reason rather than waiting out its
/// timeout.
pub(crate) fn refused_reply(reason: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({ "error": format!("{reason:#}") })
}

/// Decode one hook reply. `Err` carries the interruption or the server's
/// refusal; the caller applies its own fail policy to it.
pub fn decode_hook_reply(payload: &[u8]) -> Result<HookOutcome> {
    let value: serde_json::Value = serde_json::from_slice(payload).context("decode hook reply")?;
    if let Some(interrupted) = value.get("interrupted") {
        let interrupted: InterruptedCall = serde_json::from_value(interrupted.clone())?;
        return Err(anyhow::anyhow!(interrupted));
    }
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        anyhow::bail!("hook server refused the request: {error}");
    }
    serde_json::from_value(value["outcome"].clone()).context("deserialize hook outcome")
}
