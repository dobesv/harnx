//! # Hooks System
//!
//! Claude Code-compatible hooks for harnx. External scripts can observe and
//! influence the LLM conversation lifecycle via subprocess hooks.
//!
//! ## Protocol
//! - Hooks receive JSON on stdin (HookPayload with session_id, cwd, hook_event_name, etc.)
//! - Exit code 0: stdout parsed as JSON (HookResult) or plain text (additional_context)
//! - Exit code 2: block the operation (stderr = reason)
//! - Other exit codes: non-blocking warning
//! - Hooks marked with `async: true` run in the background and can only append context
//!   or request a follow-up turn; they never block or deny the active operation
//!
//! ## Supported Events
//! SessionStart, SessionEnd, UserPromptSubmit, Stop, StopFailure,
//! PreToolUse, PostToolUse, PostToolUseFailure, InstructionsLoaded, CwdChanged
//!
//! ## Resume
//! Stop hooks can return {"resume": true, "additionalContext": "..."} to
//! trigger another LLM turn. max_resume prevents infinite loops.
//! Async hook results are drained before each LLM turn. Results without
//! `resume: true` are queued until the next user-initiated turn, while results
//! with `resume: true` can trigger an immediate follow-up turn when the session
//! loop checks for completed async work.
//!
//! ## Permission Decision (hookSpecificOutput)
//! Hooks can return structured JSON with a `hookSpecificOutput` field to influence
//! tool execution permissions:
//! ```json
//! {
//!   "hookSpecificOutput": {
//!     "permissionDecision": "allow" | "deny" | "ask",
//!     "permissionDecisionReason": "optional reason text"
//!   }
//! }
//! ```
//! - `allow`: equivalent to exit code 0 (tool proceeds)
//! - `deny`: blocks the tool call (overrides exit code 0)
//! - `ask`: shows a confirmation prompt to the user
//!
//! This is only processed for PreToolUse hooks on exit code 0.
//!
//! Protocol follows the Claude Code hooks convention (subprocess, JSON stdin/stdout,
//! exit codes for control flow). Other coding CLIs (Gemini CLI, Cursor, etc.) use
//! similar but not identical protocols.

#[macro_use]
extern crate log;

pub mod async_manager;
pub mod dispatch;
pub mod executor;
pub mod matcher;
pub mod persistent;
pub mod prompt;
// `types` + `config` modules moved to harnx-core; re-exported below via `hooks::*`.

#[allow(unused_imports)]
pub use async_manager::{
    append_pending_context, drain_async_results, inject_pending_async_context, AsyncHookManager,
};
#[allow(unused_imports)]
pub use dispatch::{
    dispatch_hooks, dispatch_hooks_with_count, dispatch_hooks_with_count_and_manager,
    dispatch_hooks_with_managers,
};
#[allow(unused_imports)]
pub use executor::execute_command_hook;
pub use harnx_core::hooks::*;
pub use matcher::CompiledMatcher;
#[allow(unused_imports)]
pub use persistent::PersistentHookManager;
