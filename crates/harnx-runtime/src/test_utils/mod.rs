//! Test utilities moved from harnx (plan P46 T3). Contains the mock
//! LLM client (`MockClient`) and synchronisation helpers that need
//! access to the runtime `Config`/`Input`/`Client` types living in
//! this crate.
//!
//! TUI-specific harnesses (`tui_harness`, `tmux_harness`, `interrupt`,
//! `mock_openai_server`, `mock_acp`) stay in `harnx::test_utils` and
//! re-export these types via a glob shim.

pub mod mock_client;
pub mod sync;

pub use mock_client::*;
pub use sync::*;
