//! Script tools (#2813): user-defined, agent-facing tools whose body is an
//! in-process BashKit script that can call other tools.
//!
//! The provider owns the dispatch half of the shared accepted view built in
//! `config::script_tools` (decision 12): declaration selection and dispatch
//! read the same view, so a script tool disabled by a base-tool name
//! collision is neither advertised nor callable. The provider sits after the
//! `bash` provider and wraps only base providers, giving the graph
//! `bash -> script tool -> base` with no reverse edges, so script tools are
//! structurally unable to call `bash`, each other, or themselves (R10).
//!
//! Inner calls reuse the `bash` script-mode machinery wholesale: the
//! `InnerCallExecutor` for the `fork_for_inner_calls` traversal, budgets
//! (8 concurrent, 1000 calls), payload pre-flight and status classification,
//! and `ScriptInnerExecutor` + `tool_commands` for the CLI flag mapping and
//! exit codes. That reuse is what makes decision 11's Bash exit-code/stderr
//! parity hold by construction instead of by re-implementation. T3's Python
//! path runs Monty bodies through the same `InnerCallRun::call` outcomes via
//! `python.rs` host functions (`tool_input`, and `tools` through `with_tools`).

mod dispatch;
mod outcome;
mod provider;
pub(crate) mod python;
mod tool_input;

pub(crate) use provider::ScriptToolProviderConfig;
pub use provider::{ScriptEvalContextSlot, ScriptToolProvider};

#[cfg(test)]
mod tests;
