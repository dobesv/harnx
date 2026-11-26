//! Footer help text of the interaction modal, one branch per kind.

use super::InteractionModal;
use crate::tool_confirmation::TOOL_CONFIRM_IDLE_GATE;
use std::time::{Duration, Instant};

/// Footer help of the focused tray. Read/decision errors keep precedence;
/// an active footer notice (refusal or no-target feedback) renders below the
/// help so tray feedback is never invisible (review B3).
pub(super) fn interaction_footer(
    state: &InteractionModal,
    read_error: Option<&str>,
    notice: Option<&str>,
) -> String {
    let read_error = read_error.filter(|_| state.child.is_none());
    let interrupt = if state.child.is_some() {
        "interrupt child"
    } else {
        "interrupt"
    };
    let help = help_for(state, interrupt);
    match read_error.or(state.error.as_deref()) {
        Some(error) => format!("{help}\n{error}"),
        None => match notice {
            Some(notice) => format!("{help}\n{notice}"),
            None => help,
        },
    }
}

fn help_for(state: &InteractionModal, interrupt: &str) -> String {
    if state.submitting {
        return format!("Submitting… · Ctrl+C {interrupt}");
    }
    if state.a2ui.is_some() {
        return super::a2ui_render::footer_help(interrupt, active_guard_remaining(state));
    }
    if state.pending.kind == "url" {
        return url_help(state, interrupt);
    }
    if state.pending.kind != "confirm" {
        return deny_help(state, interrupt);
    }
    confirm_help(state, interrupt)
}

/// Seconds left on the reading→tray return-safety guard while it holds.
/// While it does, decisions stay refused until two seconds pass without any
/// key or paste (binding review clarification).
fn active_guard_remaining(state: &InteractionModal) -> Option<f32> {
    let deadline = state.decision_guard_until?;
    let now = Instant::now();
    (now < deadline).then(|| (deadline - now).as_secs_f32())
}

/// The idle gate keeps Enter inert for [`TOOL_CONFIRM_IDLE_GATE`] after the
/// last keypress; the label counts the remainder down so that is visible.
fn idle_gate_label(idle: Duration, verb: &str) -> String {
    if idle < TOOL_CONFIRM_IDLE_GATE {
        format!(
            "ENTER {verb} ({:.1}s)",
            TOOL_CONFIRM_IDLE_GATE.saturating_sub(idle).as_secs_f32()
        )
    } else {
        format!("ENTER {verb}")
    }
}

fn deny_help(state: &InteractionModal, interrupt: &str) -> String {
    match active_guard_remaining(state) {
        Some(secs) => format!(
            "Ctrl+D deny unlocks in {secs:.1}s of quiet · Ctrl+C {interrupt} · PgUp/PgDn scroll"
        ),
        None => format!("Ctrl+D deny · Ctrl+C {interrupt} · PgUp/PgDn scroll"),
    }
}

fn url_help(state: &InteractionModal, interrupt: &str) -> String {
    match active_guard_remaining(state) {
        Some(secs) => format!(
            "ENTER/Ctrl+D unlock in {secs:.1}s of quiet keys · Ctrl+C {interrupt} · PgUp/PgDn scroll"
        ),
        None => {
            let idle = Instant::now().saturating_duration_since(state.last_key_at);
            format!(
                "{} · Ctrl+D cancel · Ctrl+C {interrupt} · PgUp/PgDn scroll",
                idle_gate_label(idle, "continue")
            )
        }
    }
}

fn confirm_help(state: &InteractionModal, interrupt: &str) -> String {
    match active_guard_remaining(state) {
        Some(secs) => format!(
            "ENTER/Ctrl+D unlock in {secs:.1}s of quiet keys · Ctrl+C {interrupt} · Shift+Enter newline · PgUp/PgDn scroll"
        ),
        None => {
            let idle = Instant::now().saturating_duration_since(state.last_key_at);
            format!(
                "{} · Ctrl+D deny · Ctrl+C {interrupt} · Shift+Enter newline · PgUp/PgDn scroll",
                idle_gate_label(idle, "approve")
            )
        }
    }
}
