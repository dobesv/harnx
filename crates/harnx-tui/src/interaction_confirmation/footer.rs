//! Footer help text of the interaction modal, one branch per kind.

use super::InteractionModal;
use crate::tool_confirmation::TOOL_CONFIRM_IDLE_GATE;
use std::time::{Duration, Instant};

pub(super) fn interaction_footer(state: &InteractionModal, read_error: Option<&str>) -> String {
    let read_error = read_error.filter(|_| state.child.is_none());
    let interrupt = if state.child.is_some() {
        "interrupt child"
    } else {
        "interrupt"
    };
    let help = help_for(state, interrupt);
    match read_error.or(state.error.as_deref()) {
        Some(error) => format!("{help}\n{error}"),
        None => help,
    }
}

fn help_for(state: &InteractionModal, interrupt: &str) -> String {
    if state.submitting {
        format!("Submitting… · Ctrl+C {interrupt}")
    } else if state.a2ui.is_some() {
        super::a2ui_render::footer_help(interrupt)
    } else if state.pending.kind == "url" {
        url_help(state, interrupt)
    } else if state.pending.kind != "confirm" {
        format!("Ctrl+D deny · Ctrl+C {interrupt} · PgUp/PgDn scroll")
    } else {
        confirm_help(state, interrupt)
    }
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

fn url_help(state: &InteractionModal, interrupt: &str) -> String {
    let idle = Instant::now().saturating_duration_since(state.last_key_at);
    format!(
        "{} · Ctrl+D cancel · Ctrl+C {interrupt} · PgUp/PgDn scroll",
        idle_gate_label(idle, "continue")
    )
}

fn confirm_help(state: &InteractionModal, interrupt: &str) -> String {
    let idle = Instant::now().saturating_duration_since(state.last_key_at);
    format!(
        "{} · Ctrl+D deny · Ctrl+C {interrupt} · Shift+Enter newline · PgUp/PgDn scroll",
        idle_gate_label(idle, "approve")
    )
}
