//! Structured "notice" channel for surfacing hook messages to the harnx UI.
//!
//! A hook (or an exec sub-hook) emits a standalone JSONL line on stdout:
//!
//! ```json
//! {"notice": {"level": "error", "message": "…"}}
//! ```
//!
//! These bubble up asynchronously (they carry no request `id`, so they are not
//! confused with request/response messages). Exec sub-hooks call [`send`] when
//! they read such a line from their child; the JSONL loop drains the receiver
//! returned by [`init_channel`] and re-emits the line on proxy-auth's own
//! stdout, where harnx recognizes it and posts an `AgentEvent::Notice`.

use std::sync::OnceLock;

use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone)]
pub struct HookNotice {
    pub level: String,
    pub message: String,
}

/// Receiver end drained by the JSONL loop.
pub type HookNoticeReceiver = UnboundedReceiver<HookNotice>;

static NOTICE_TX: OnceLock<UnboundedSender<HookNotice>> = OnceLock::new();

/// Create the process-wide notice channel and register the sender. Returns the
/// receiver for the JSONL loop to drain. Idempotent-safe: a second call returns
/// a fresh (unregistered) receiver, but only the first sender is kept.
pub fn init_channel() -> UnboundedReceiver<HookNotice> {
    let (tx, rx) = unbounded_channel();
    let _ = NOTICE_TX.set(tx);
    rx
}

/// Queue a notice to bubble up to harnx. No-op if no channel is registered
/// (e.g. proxy-auth invoked outside the persistent-hook JSONL loop).
pub fn send(level: &str, message: &str) {
    if let Some(tx) = NOTICE_TX.get() {
        let _ = tx.send(HookNotice {
            level: level.to_string(),
            message: message.to_string(),
        });
    }
}

/// If `value` is a standalone notice line (`{"notice": {"message": …}}`),
/// return it. `level` defaults to `warning` when absent.
pub fn parse_notice_line(value: &Value) -> Option<HookNotice> {
    let notice = value.get("notice")?.as_object()?;
    let message = notice.get("message")?.as_str()?.trim();
    if message.is_empty() {
        return None;
    }
    let level = notice
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("warning");
    Some(HookNotice {
        level: level.to_string(),
        message: message.to_string(),
    })
}

/// Render a notice as its JSONL wire form (without trailing newline).
pub fn to_line(notice: &HookNotice) -> String {
    serde_json::json!({
        "notice": {"level": notice.level, "message": notice.message}
    })
    .to_string()
}
