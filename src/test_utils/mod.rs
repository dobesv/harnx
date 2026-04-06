//! Test utilities for end-to-end and integration tests.
//!
//! This module provides shared helpers for test-only infrastructure, including
//! mock clients, TUI harnesses, synchronization primitives, and ACP test
//! servers. Implementations are added in follow-up tasks.

pub mod mock_acp;
pub mod mock_client;
pub mod sync;
pub mod tui_harness;

pub use mock_acp::*;
pub use mock_client::*;
pub use sync::*;
pub use tui_harness::*;
