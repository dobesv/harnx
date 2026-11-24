//! Names of the operations registered by lifecycle event hooks.

use harnx_core::hooks::HookEventKind;

pub(crate) fn names(event: HookEventKind) -> &'static [&'static str] {
    match event {
        HookEventKind::SessionStart => &["event_input", "fetch", "add_context"],
        HookEventKind::UserPromptSubmit => &[
            "event_input",
            "fetch",
            "add_context",
            "block",
            "rewrite_prompt",
        ],
        HookEventKind::Stop => &["event_input", "fetch", "resume"],
        HookEventKind::StopFailure => &["event_input", "fetch"],
    }
}
