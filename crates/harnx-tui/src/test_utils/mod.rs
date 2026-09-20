//! TUI-specific test harnesses. Moved from `harnx::test_utils::tui_harness`
//! (plan P49).

#[cfg(test)]
mod environment;
#[cfg(test)]
mod exit_cancel;
pub mod tui_harness;

#[cfg(test)]
pub(crate) use environment::*;
#[cfg(test)]
pub(crate) use exit_cancel::*;
pub use tui_harness::*;
