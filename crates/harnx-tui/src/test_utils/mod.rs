//! TUI-specific test harnesses. Moved from `harnx::test_utils::tui_harness`
//! (plan P49).

#[cfg(test)]
mod environment;
pub mod tui_harness;

#[cfg(test)]
pub(crate) use environment::*;
pub use tui_harness::*;
