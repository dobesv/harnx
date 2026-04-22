//! harnx-runtime — Config-aware runtime glue extracted from the `harnx`
//! crate (plan P46, β+ progressive peel). Holds the `Config`, `Session`,
//! `Agent`, `Input` runtime types plus the provider-client orchestration
//! (`call_chat_completions`, retry/fallback), dot-commands dispatch,
//! and the `ToolEvalContext` bridge to `harnx-engine`.
//!
//! Downstream front-end crates (`harnx-serve`, `harnx-acp-server`,
//! `harnx-tui`) depend on this crate rather than on `harnx` directly.
