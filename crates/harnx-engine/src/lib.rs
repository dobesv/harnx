//! `harnx-engine` is the crate that will own the unified LLM + tool-call
//! loop for harnx. In this initial scaffold it exposes only the skeleton
//! of `Engine::run_turn` — a function that returns a `Stream` of
//! `AgentEvent` values and emits no LLM or tool activity yet. Real
//! behavior is migrated in from `harnx/src/client/retry.rs` and
//! `harnx/src/tool.rs` in a later plan.
//!
//! The design is described in detail at
//! `docs/superpowers/specs/2026-04-19-monorepo-refactor-design.md`
//! (§3 Engine API, §4 I/O discipline).

pub mod chat_completions;
pub mod engine;
pub mod input;
pub mod retry;
pub mod tool;

pub use engine::Engine;
pub use input::EngineInput;
